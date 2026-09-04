use super::*;
use crate::telemetry::log_record::{CommandLocation, LogScope};

fn event(message: &str) -> LogEvent {
    LogEvent {
        namespace: "build.stdout".into(),
        message: message.into(),
        scope: Some(LogScope {
            component: crate::domain::ComponentName::parse("api").unwrap(),
            step: "build-package".into(),
        }),
        kind: LogEventKind::Output,
    }
}

#[test]
fn json_is_validated_before_field_redaction_and_output_never_becomes_metadata() {
    let record = LogRecord {
        version: LOG_RECORD_VERSION,
        elapsed_ms: 7,
        fragment: None,
        event: event("{\"version\":1}\nprivate TOKEN"),
    };
    let json = serde_json::to_vec(&record).unwrap();
    let redactor = Redactor::new(["TOKEN".into(), "\"version\":1".into()]);
    let safe = decode_log_record(&json, &redactor).unwrap();
    assert_eq!(safe.version, LOG_RECORD_VERSION);
    assert_eq!(safe.event.kind, LogEventKind::Output);
    assert_eq!(safe.event.message, "{[REDACTED]}\nprivate [REDACTED]");
    let encoded = encode_log_record(&safe, &redactor).unwrap();
    assert_eq!(encoded.lines().count(), 1);
    assert_eq!(
        decode_log_record(encoded.as_bytes(), &redactor).unwrap(),
        safe
    );
}

#[test]
fn malformed_oversized_and_future_records_are_not_plain_text_fallbacks() {
    assert_eq!(
        decode_log_record(b"not-json", &Redactor::default()),
        Err(LogCodecError::Invalid)
    );
    let mut record = LogRecord {
        version: 2,
        elapsed_ms: 0,
        fragment: None,
        event: event("safe"),
    };
    assert_eq!(
        decode_log_record(&serde_json::to_vec(&record).unwrap(), &Redactor::default()),
        Err(LogCodecError::UnsupportedVersion)
    );
    record.version = LOG_RECORD_VERSION;
    record.event.message = "x".repeat(MAX_MESSAGE_BYTES + 1);
    assert_eq!(
        encode_log_record(&record, &Redactor::default()),
        Err(LogCodecError::Limit)
    );
    assert_eq!(
        decode_log_record(&vec![b' '; MAX_RECORD_BYTES + 1], &Redactor::default()),
        Err(LogCodecError::Limit)
    );
    let json = b"{\"version\":1,\"elapsed_ms\":0,\"event\":{},\"command\":\"injected\"}";
    assert_eq!(
        decode_log_record(json, &Redactor::default()),
        Err(LogCodecError::Invalid)
    );
}

#[test]
fn long_multibyte_output_is_complete_scoped_and_bounded_after_splitting() {
    let message = format!("{}TAIL", "文".repeat(MAX_MESSAGE_BYTES));
    let original = event(&message);
    let fragments = split_log_event(&original, &Redactor::default()).unwrap();
    assert!(fragments.len() > 1);
    assert_eq!(
        fragments
            .iter()
            .map(|event| event.message.as_str())
            .collect::<String>(),
        message
    );
    for fragment in fragments {
        assert_eq!(fragment.scope, original.scope);
        assert!(fragment.message.len() <= MAX_MESSAGE_BYTES);
        let record = LogRecord {
            version: 1,
            elapsed_ms: 1,
            fragment: None,
            event: fragment,
        };
        assert!(
            encode_log_record(&record, &Redactor::default())
                .unwrap()
                .len()
                <= MAX_RECORD_BYTES
        );
    }
}

#[test]
fn named_command_secrets_are_masked_and_oversized_snapshots_are_unavailable() {
    let mut original = event("failed");
    original.kind = LogEventKind::FailedCommand {
        command: RecordedCommand {
            location: super::super::CommandLocation::Local,
            index: Some(2),
            program: "tool".into(),
            args: vec![
                "--api-key".into(),
                "named-secret".into(),
                "PASSWORD=another-secret".into(),
                "Authorization: Bearer header-secret".into(),
                "public".into(),
            ],
        },
    };
    let safe = sanitize_log_event(&original, &Redactor::default()).unwrap();
    let LogEventKind::FailedCommand { command } = safe.kind else {
        panic!("safe complete argv expected");
    };
    assert_eq!(
        command.args,
        [
            "--api-key",
            "[REDACTED]",
            "PASSWORD=[REDACTED]",
            "[REDACTED]",
            "public"
        ]
    );
    let LogEventKind::FailedCommand { command } = &mut original.kind else {
        unreachable!()
    };
    command.args = vec!["argument".into(); MAX_COMMAND_ARGUMENTS + 1];
    assert_eq!(
        sanitize_log_event(&original, &Redactor::default())
            .unwrap()
            .kind,
        LogEventKind::CommandUnavailable {
            location: CommandLocation::Local,
            index: Some(2)
        }
    );
}

#[test]
fn control_removal_cannot_join_secret_bytes_and_private_keys_are_never_released() {
    let input = "TO\u{200b}KEN \u{1b}[31m -----BEGIN PRIVATE KEY-----\nbody\n-----END PRIVATE KEY-----\nend";
    let safe = sanitize_log_text(input, &Redactor::new(["TOKEN".into()]), 1024).unwrap();
    assert!(!safe.contains("TOKEN"));
    assert!(!safe.contains("body"));
    assert!(!safe.contains('\u{1b}'));
    assert!(!safe.contains('\u{200b}'));
    assert!(safe.contains("[REDACTED PRIVATE KEY]"));
}

#[test]
fn invalid_fragment_counts_indices_and_nonoutput_continuations_are_rejected() {
    use crate::telemetry::log_record::LogFragment;
    let mut record = LogRecord {
        version: LOG_RECORD_VERSION,
        elapsed_ms: 1,
        fragment: None,
        event: event("safe"),
    };
    for (index, count) in [(0, 1), (2, 2), (0, 1026)] {
        record.fragment = Some(LogFragment {
            event_id: uuid::Uuid::now_v7(),
            index,
            count,
        });
        assert_eq!(
            encode_log_record(&record, &Redactor::default()),
            Err(LogCodecError::Invalid)
        );
    }
    record.fragment = Some(LogFragment {
        event_id: uuid::Uuid::now_v7(),
        index: 1,
        count: 2,
    });
    record.event.kind = LogEventKind::CommandUnavailable {
        location: CommandLocation::Local,
        index: Some(1),
    };
    assert_eq!(
        decode_log_record(&serde_json::to_vec(&record).unwrap(), &Redactor::default()),
        Err(LogCodecError::Invalid)
    );
}

#[test]
fn isolated_fragment_body_is_hidden_and_expanded_public_record_is_rejected() {
    let fragment = serde_json::json!({"version":1,"elapsed_ms":1,"fragment":{"event_id":uuid::Uuid::now_v7(),"index":1,"count":2},"event":{"namespace":"test.output","message":"private-suffix\n-----END PRIVATE KEY-----","scope":null,"kind":{"kind":"output"}}});
    let decoded = decode_log_record(
        &serde_json::to_vec(&fragment).unwrap(),
        &Redactor::default(),
    )
    .unwrap();
    assert!(!decoded.event.message.contains("private-suffix"));
    let record = serde_json::json!({"version":1,"elapsed_ms":1,"event":{"namespace":"test.output","message":"x".repeat(MAX_MESSAGE_BYTES),"scope":null,"kind":{"kind":"output"}}});
    assert_eq!(
        decode_log_record(
            &serde_json::to_vec(&record).unwrap(),
            &Redactor::new(["x".into()])
        ),
        Err(LogCodecError::Limit)
    );
}

#[test]
fn registered_marker_words_cannot_destroy_private_key_detection_before_normalization() {
    let redactor = Redactor::new(["BEGIN".into(), "TO\u{200b}KEN".into()]);
    let raw = "TO\u{200b}KEN -----BE\u{200b}GIN PRIVATE KEY-----\nprivate-body\n-----END PRIVATE KEY-----\nafter";
    let safe = sanitize_log_text(raw, &redactor, 4096).unwrap();
    assert!(!safe.contains("TOKEN"));
    assert!(!safe.contains("private-body"));
    assert!(safe.contains("[REDACTED PRIVATE KEY]"));
    assert!(safe.ends_with("after"));
}

#[test]
fn normalized_secrets_are_deduplicated_and_redacted_longest_first() {
    let redactor = Redactor::new([
        format!("token{}", "\u{200b}".repeat(30)),
        "token-value".into(),
        "token\u{200b}-value".into(),
        "\u{200b}".into(),
    ]);
    assert_eq!(
        sanitize_log_text("token-value token", &redactor, 4096).unwrap(),
        "[REDACTED] [REDACTED]"
    );
}

#[test]
fn hidden_controls_in_named_flags_and_sensitive_component_identity_do_not_leak() {
    let mut original = event("safe");
    original.scope.as_mut().unwrap().component =
        crate::domain::ComponentName::parse("token").unwrap();
    original.kind = LogEventKind::FailedCommand {
        command: RecordedCommand {
            location: CommandLocation::Local,
            index: Some(1),
            program: "tool".into(),
            args: vec![
                "--to\u{200b}ken=VALUE1".into(),
                "-HAuthorization: VALUE2".into(),
                "--password VALUE3".into(),
            ],
        },
    };
    let safe = sanitize_log_event(&original, &Redactor::new(["token".into()])).unwrap();
    assert!(safe.scope.is_none());
    let text = serde_json::to_string(&safe).unwrap();
    for value in ["token", "VALUE1", "VALUE2", "VALUE3"] {
        assert!(!text.contains(value));
    }
}
