use super::*;
use crate::{
    history::HistoryStore,
    telemetry::{
        Redactor,
        log_record::{
            LOG_RECORD_VERSION, LogEvent, LogEventKind, LogScope, MAX_MESSAGE_BYTES,
            encode_log_record,
        },
    },
};
use std::path::PathBuf;

struct Fixture {
    directory: tempfile::TempDir,
    service: HistoryQueryService,
    store: HistoryStore,
    scope: HistoryLogScope,
}

impl Fixture {
    fn new(format: Option<DeploymentLogFormat>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let store = HistoryStore::open(&path).unwrap();
        let scope = HistoryLogScope {
            project: ProjectId::new(),
            environment: EnvironmentId::new(),
            deployment: DeploymentId::new(),
        };
        store
            .create_deployment(&scope.deployment, &scope.project, &scope.environment, 1)
            .unwrap();
        match format {
            Some(DeploymentLogFormat::JsonlV1) => {
                store
                    .register_event_log(&scope.deployment, MAX_LOG_BYTES, 3)
                    .unwrap();
            }
            Some(DeploymentLogFormat::LegacyText) => {
                store
                    .register_deployment_log(&scope.deployment, MAX_LOG_BYTES, 3)
                    .unwrap();
            }
            None => {}
        }
        Self {
            service: HistoryQueryService::new(path, Redactor::new(["TOKEN".into()])),
            directory,
            store,
            scope,
        }
    }

    fn write(&self, generation: u32, text: &str) -> PathBuf {
        let directory = self.directory.path().join("logs");
        fs::create_dir_all(&directory).unwrap();
        let path = log_path(&directory, &self.scope.deployment, generation);
        fs::write(&path, text).unwrap();
        path
    }

    fn read(&self, query: LogReadQuery) -> Result<LogReadPage, HistoryQueryError> {
        self.service
            .read_logs(&self.scope, query, &CancellationToken::new())
    }
}

fn event(component: &str, step: &str, text: &str) -> String {
    encode_log_record(
        &LogRecord {
            version: LOG_RECORD_VERSION,
            elapsed_ms: 1,
            fragment: None,
            event: LogEvent {
                namespace: "test.output".into(),
                message: text.into(),
                scope: Some(LogScope {
                    component: ComponentName::parse(component).unwrap(),
                    step: step.into(),
                }),
                kind: LogEventKind::Output,
            },
        },
        &Redactor::default(),
    )
    .unwrap()
}

fn fragment_records(message: &str) -> Vec<String> {
    use crate::telemetry::log_record::{LogFragment, split_log_event};
    let original: LogRecord = serde_json::from_str(&event("api", "prepare", "")).unwrap();
    let mut event = original.event;
    event.message = message.into();
    let events = split_log_event(&event, &Redactor::default()).unwrap();
    let count = u32::try_from(events.len()).unwrap();
    let event_id = uuid::Uuid::now_v7();
    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            encode_log_record(
                &LogRecord {
                    version: LOG_RECORD_VERSION,
                    elapsed_ms: 1,
                    fragment: Some(LogFragment {
                        event_id,
                        index: u32::try_from(index).unwrap(),
                        count,
                    }),
                    event,
                },
                &Redactor::default(),
            )
            .unwrap()
        })
        .collect()
}

fn raw_fragment(id: uuid::Uuid, index: u32, count: u32, message: &str) -> String {
    let value = serde_json::json!({"version":1,"elapsed_ms":1,"fragment":{"event_id":id,"index":index,"count":count},"event":{"namespace":"test.output","message":message,"scope":{"component":"api","step":"prepare"},"kind":{"kind":"output"}}});
    format!("{value}\n")
}

#[test]
fn raw_indexed_private_key_fragments_are_joined_before_any_body_redaction() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let id = uuid::Uuid::now_v7();
    let first = raw_fragment(
        id,
        0,
        2,
        "before\n-----BEGIN PRIVATE KEY-----\nprivate-prefix\n",
    );
    let last = raw_fragment(id, 1, 2, "private-suffix\n-----END PRIVATE KEY-----\nafter");
    let path = fixture.write(1, &first);
    fixture.write(0, &last);
    assert!(fs::read_to_string(path).unwrap().contains("private-prefix"));
    let page = fixture.read(LogReadQuery::default()).unwrap();
    let safe = page
        .entries
        .iter()
        .map(LogEntry::message)
        .collect::<String>();
    assert!(safe.contains("before\n[REDACTED PRIVATE KEY]\nafter"));
    assert!(!safe.contains("private-prefix"));
    assert!(!safe.contains("private-suffix"));
    assert!(page.coverage.matches_are_complete);
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.source_generations == [1, 0])
    );
    let export = fixture
        .service
        .prepare_log_export(
            &fixture.scope,
            &LogFilter::default(),
            LogExportFormat::Json,
            &CancellationToken::new(),
        )
        .unwrap();
    let text = std::str::from_utf8(export.payload()).unwrap();
    assert!(!text.contains("private-prefix"));
    assert!(!text.contains("private-suffix"));
    let json: serde_json::Value = serde_json::from_slice(export.payload()).unwrap();
    assert!(json["entries"].as_array().unwrap().iter().all(|entry| {
        entry["source_generations"] == serde_json::json!([1, 0])
            && entry.get("sanitized_view_fragment").is_some()
            && entry.get("ordinal").is_none()
    }));
}

#[test]
fn expanded_structured_group_keeps_all_source_generations_for_each_safe_view_piece() {
    let mut fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    fixture.service = HistoryQueryService::new(
        fixture.directory.path().join("history.sqlite3"),
        Redactor::new(["x".into()]),
    );
    let id = uuid::Uuid::now_v7();
    fixture.write(1, &raw_fragment(id, 0, 2, &"x".repeat(8192)));
    fixture.write(0, &raw_fragment(id, 1, 2, &"x".repeat(8192)));
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(page.entries.len() > 2);
    for (index, entry) in page.entries.iter().enumerate() {
        assert_eq!(entry.source_generations, [1, 0]);
        assert_eq!(entry.generation, 1);
        assert_eq!(entry.ordinal, u64::try_from(index + 1).unwrap());
    }
    assert_eq!(
        page.entries
            .iter()
            .map(LogEntry::message)
            .collect::<String>(),
        "[REDACTED]".repeat(16384)
    );
}

#[test]
fn raw_orphan_suffix_and_incomplete_prefix_bodies_are_hidden_not_individually_sanitized() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let id = uuid::Uuid::now_v7();
    fixture.write(
        0,
        &(raw_fragment(id, 1, 3, "private-suffixTO")
            + &raw_fragment(id, 2, 3, "KEN\n-----END PRIVATE KEY-----")),
    );
    let page = fixture.read(LogReadQuery::default()).unwrap();
    let text = page
        .entries
        .iter()
        .map(LogEntry::message)
        .collect::<String>();
    assert!(!text.contains("private-suffix"));
    assert!(!text.contains("TOKEN"));
    assert!(!text.contains("KEN"));
    assert!(text.contains("Fragment body unavailable"));
    assert!(!page.coverage.matches_are_complete);
    fixture.write(0, &raw_fragment(id, 0, 3, "unconfirmed-prefix-body"));
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(
        !page
            .entries
            .iter()
            .any(|entry| entry.message().contains("unconfirmed-prefix-body"))
    );
    assert!(!page.coverage.matches_are_complete);
}

#[test]
fn reader_redaction_expansion_is_safely_refragmented_before_paging() {
    let mut fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    fixture.service = HistoryQueryService::new(
        fixture.directory.path().join("history.sqlite3"),
        Redactor::new(["x".into()]),
    );
    let value = serde_json::json!({"version":1,"elapsed_ms":1,"event":{"namespace":"test.output","message":"x".repeat(MAX_MESSAGE_BYTES),"scope":{"component":"api","step":"prepare"},"kind":{"kind":"output"}}});
    fixture.write(0, &format!("{value}\n"));
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(page.entries.len() > 1);
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.message().len() <= MAX_MESSAGE_BYTES)
    );
    assert_eq!(
        page.entries
            .iter()
            .map(LogEntry::message)
            .collect::<String>(),
        "[REDACTED]".repeat(MAX_MESSAGE_BYTES)
    );
    assert!(page.coverage.matches_are_complete);
    assert!(page.next_cursor.is_none());
}

#[test]
fn raw_legacy_rotation_run_redacts_cross_file_pem_and_secret_before_search_or_export() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(2, "before TO");
    fixture.write(1, "KEN\n-----BEGIN PRIVATE KEY-----\nprivate-prefix\n");
    fixture.write(0, "private-suffix\n-----END PRIVATE KEY-----\nafter");
    let page = fixture.read(LogReadQuery::default()).unwrap();
    let text = page
        .entries
        .iter()
        .map(LogEntry::message)
        .collect::<String>();
    assert_eq!(text, "before [REDACTED]\n[REDACTED PRIVATE KEY]\nafter");
    assert!(page.coverage.matches_are_complete);
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.source_generations == [2, 1, 0])
    );
    let filtered = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                text: "private-suffix".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert!(filtered.entries.is_empty());
    for format in [LogExportFormat::Text, LogExportFormat::Json] {
        let exported = fixture
            .service
            .prepare_log_export(
                &fixture.scope,
                &LogFilter::default(),
                format,
                &CancellationToken::new(),
            )
            .unwrap();
        let text = std::str::from_utf8(exported.payload()).unwrap();
        assert!(!text.contains("TOKEN"));
        assert!(!text.contains("private-prefix"));
        assert!(!text.contains("private-suffix"));
        assert!(text.contains("source_generations") || text.contains("source generations"));
    }
    assert!(matches!(
        fixture.service.log_page(
            &fixture.scope.project,
            &fixture.scope.environment,
            &fixture.scope.deployment,
            super::super::HistoricalLogQuery::default()
        ),
        Err(HistoryQueryError::HistoryUnavailable)
    ));
}

#[test]
fn legacy_orphan_end_and_runs_after_generation_gap_hide_unknown_bodies() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(0, "orphan-private-suffix\n-----END PRIVATE KEY-----\nafter");
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(!page.coverage.matches_are_complete);
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::IncompleteLegacy { generation: 0 })
    );
    assert!(
        !page
            .entries
            .iter()
            .any(|entry| entry.message().contains("orphan-private-suffix"))
    );
    fixture.write(2, "before\n-----BEGIN PRIVATE KEY-----\nprivate-prefix");
    fixture.write(0, "private-body-after-gap-without-visible-end");
    let page = fixture.read(LogReadQuery::default()).unwrap();
    let text = page
        .entries
        .iter()
        .map(LogEntry::message)
        .collect::<String>();
    assert!(!text.contains("private-body-after-gap"));
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::GenerationGap)
    );
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::IncompleteLegacy { generation: 0 })
    );
    assert!(!page.coverage.matches_are_complete);
}

#[test]
fn legacy_extra_hyphen_orphan_end_never_exposes_prefix_body() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    for marker in [
        "------END PRIVATE KEY-----",
        "-------END PRIVATE KEY-----",
        "------E\u{200b}ND RSA PRIVATE KEY-----",
    ] {
        fixture.write(0, &format!("private-suffix\n{marker}\nafter"));
        let page = fixture.read(LogReadQuery::default()).unwrap();
        assert!(!page.coverage.matches_are_complete);
        assert!(
            page.coverage
                .issues
                .contains(&LogCoverageIssue::IncompleteLegacy { generation: 0 })
        );
        assert!(
            !page
                .entries
                .iter()
                .any(|entry| entry.message().contains("private-suffix"))
        );
        assert!(matches!(
            compatibility_legacy_page(&fixture.service, &fixture.scope, 0),
            Err(HistoryQueryError::HistoryUnavailable)
        ));
        let exported = fixture
            .service
            .prepare_log_export(
                &fixture.scope,
                &LogFilter::default(),
                LogExportFormat::Json,
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(
            !std::str::from_utf8(exported.payload())
                .unwrap()
                .contains("private-suffix")
        );
    }
}

#[test]
fn compatibility_legacy_page_returns_safe_bytes_from_its_own_verified_scan() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(1, "-----BEGIN PRIVATE KEY-----\n");
    let path = fixture.write(0, "old-private-body\n-----END PRIVATE KEY-----");
    let earlier = fs::read_to_string(&path).unwrap();
    assert!(earlier.contains("old-private-body"));
    // Replace both files after the caller's earlier observation. The helper
    // returns only the new scan's text, never an authorization for `earlier`.
    fixture.write(1, "new previous\n");
    fixture.write(0, "new active TOKEN\n");
    let (safe, available) = compatibility_legacy_page(&fixture.service, &fixture.scope, 0).unwrap();
    assert_eq!(safe, "new active [REDACTED]\n");
    assert_eq!(available, [0, 1]);
    assert!(!safe.contains("old-private-body"));
    let old = fixture
        .service
        .log_page(
            &fixture.scope.project,
            &fixture.scope.environment,
            &fixture.scope.deployment,
            super::super::HistoricalLogQuery::default(),
        )
        .unwrap();
    assert_eq!(old.text, safe);
    assert!(matches!(
        compatibility_legacy_page(&fixture.service, &fixture.scope, 2),
        Err(HistoryQueryError::LogChanged)
    ));
}

#[test]
fn ordinary_legacy_rotations_keep_compatibility_pages_and_match_across_boundaries() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(1, "before ABC");
    fixture.write(0, "DEF after");
    let page = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                text: "cDe".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(LogEntry::message)
            .collect::<String>(),
        "before ABCDEF after"
    );
    assert_eq!(page.entries[0].source_generations, [1, 0]);
    let old = fixture
        .service
        .log_page(
            &fixture.scope.project,
            &fixture.scope.environment,
            &fixture.scope.deployment,
            super::super::HistoricalLogQuery::default(),
        )
        .unwrap();
    assert_eq!(old.text, "DEF after");
}

#[test]
fn explicit_fragments_search_across_rotation_and_secret_boundaries() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let message = format!(
        "{}AbCdE{}TAIL",
        "x".repeat(MAX_MESSAGE_BYTES - 2),
        "y".repeat(200)
    );
    let pieces = fragment_records(&message);
    assert_eq!(pieces.len(), 2);
    fixture.write(1, &pieces[0]);
    fixture.write(0, &pieces[1]);
    let page = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                text: "aBcDe".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(LogEntry::message)
            .collect::<String>(),
        message
    );
    assert!(page.coverage.matches_are_complete);
    let secret = format!("{}TOKEN after", "x".repeat(MAX_MESSAGE_BYTES - 2));
    let pieces = fragment_records(&secret);
    fixture.write(1, &pieces[0]);
    fixture.write(0, &pieces[1]);
    let page = fixture.read(LogReadQuery::default()).unwrap();
    let safe = page
        .entries
        .iter()
        .map(LogEntry::message)
        .collect::<String>();
    assert_eq!(
        safe,
        format!("{}[REDACTED] after", "x".repeat(MAX_MESSAGE_BYTES - 2))
    );
    let export = fixture
        .service
        .prepare_log_export(
            &fixture.scope,
            &LogFilter::default(),
            LogExportFormat::Json,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(
        !std::str::from_utf8(export.payload())
            .unwrap()
            .contains("TOKEN")
    );
}

#[test]
fn missing_or_mismatched_fragments_are_never_guessed_complete() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let pieces = fragment_records(&format!("{}END", "x".repeat(MAX_MESSAGE_BYTES)));
    fixture.write(0, &pieces[1]);
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::IncompleteFragments { generation: 0 })
    );
    assert!(!page.coverage.matches_are_complete);
    let mut mismatched: LogRecord = serde_json::from_str(&pieces[1]).unwrap();
    mismatched.event.scope.as_mut().unwrap().step = "different".into();
    fixture.write(
        0,
        &(pieces[0].clone() + &encode_log_record(&mismatched, &Redactor::default()).unwrap()),
    );
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(!page.coverage.matches_are_complete);
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::IncompleteFragments { generation: 0 })
    );
}

#[test]
fn same_size_content_change_with_restored_time_invalidates_cursor() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let path = fixture.write(
        0,
        &(event("api", "prepare", "first") + &event("api", "prepare", "later")),
    );
    let time = fs::metadata(&path).unwrap().modified().unwrap();
    let first = fixture
        .read(LogReadQuery {
            limit: 1,
            ..LogReadQuery::default()
        })
        .unwrap();
    fixture.write(
        0,
        &(event("api", "prepare", "other") + &event("api", "prepare", "later")),
    );
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
    assert!(matches!(
        fixture.read(LogReadQuery {
            cursor: first.next_cursor,
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::LogChanged)
    ));
}

#[test]
fn export_includes_safe_filter_and_refuses_instead_of_truncating_large_payload() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(0, "TOKEN matching VALUE");
    let filter = LogFilter {
        text: "VaLuE".into(),
        ..LogFilter::default()
    };
    for format in [LogExportFormat::Json, LogExportFormat::Text] {
        let exported = fixture
            .service
            .prepare_log_export(&fixture.scope, &filter, format, &CancellationToken::new())
            .unwrap();
        let text = std::str::from_utf8(exported.payload()).unwrap();
        assert!(text.contains("value"));
        assert!(text.contains("[REDACTED] matching VALUE"));
    }
    fixture.write(1, &"x".repeat(5 * 1024 * 1024));
    fixture.write(0, &"y".repeat(5 * 1024 * 1024));
    assert!(matches!(
        fixture.service.prepare_log_export(
            &fixture.scope,
            &LogFilter::default(),
            LogExportFormat::Text,
            &CancellationToken::new()
        ),
        Err(HistoryQueryError::LogLimit)
    ));
}

#[test]
fn scans_all_retained_generations_before_paging_and_searching() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    fixture.write(2, &event("api", "prepare", "oldest TOKEN match"));
    fixture.write(1, &event("worker", "prepare", "middle match"));
    fixture.write(0, &event("api", "activate", "newest match"));
    let first = fixture
        .read(LogReadQuery {
            limit: 1,
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(first.total_matches, 3);
    assert_eq!(first.entries[0].generation, 2);
    assert_eq!(first.entries[0].message(), "oldest [REDACTED] match");
    assert!(first.coverage.matches_are_complete);
    let second = fixture
        .read(LogReadQuery {
            cursor: first.next_cursor,
            limit: 1,
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(second.entries[0].generation, 1);
    let found = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                component: Some(ComponentName::parse("api").unwrap()),
                step: Some("prepare".into()),
                text: "match".into(),
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(found.total_matches, 1);
    assert_eq!(found.entries[0].generation, 2);
}

#[test]
fn cursor_rejects_changed_scope_filter_and_rotation() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    fixture.write(
        0,
        &(event("api", "prepare", "first") + &event("api", "prepare", "second")),
    );
    let cursor = fixture
        .read(LogReadQuery {
            limit: 1,
            ..LogReadQuery::default()
        })
        .unwrap()
        .next_cursor
        .unwrap();
    assert!(matches!(
        fixture.read(LogReadQuery {
            cursor: Some(cursor.clone()),
            filter: LogFilter {
                text: "second".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::LogChanged)
    ));
    let mut other = fixture.scope.clone();
    other.environment = EnvironmentId::new();
    assert!(matches!(
        fixture.service.read_logs(
            &other,
            LogReadQuery {
                cursor: Some(cursor.clone()),
                ..LogReadQuery::default()
            },
            &CancellationToken::new()
        ),
        Err(HistoryQueryError::NotFound)
    ));
    fixture.write(1, &event("api", "prepare", "rotated"));
    assert!(matches!(
        fixture.read(LogReadQuery {
            cursor: Some(cursor),
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::LogChanged)
    ));
}

#[test]
fn legacy_json_is_never_upgraded_and_scope_filter_reports_unknown() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    let spoof = event("api", "activate", "looks structured TOKEN");
    fixture.write(0, &spoof);
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert!(matches!(
        page.entries[0].content,
        LogEntryContent::Legacy(_)
    ));
    assert!(
        page.coverage
            .issues
            .contains(&LogCoverageIssue::LegacyUnscoped)
    );
    assert!(!page.entries[0].message().contains("TOKEN"));
    let filtered = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                component: Some(ComponentName::parse("api").unwrap()),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert!(filtered.entries.is_empty());
    assert!(!filtered.coverage.matches_are_complete);
}

#[test]
fn missing_torn_unknown_version_and_invalid_lines_never_mean_complete_no_match() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let missing = fixture.read(LogReadQuery::default()).unwrap();
    assert_eq!(missing.coverage.status, HistoricalLogStatus::Missing);
    assert!(!missing.coverage.matches_are_complete);
    fixture.write(
        2,
        "{\"version\":99,\"future\":true}\nnot-json\n{\"version\":1",
    );
    fixture.write(0, &event("api", "prepare", "unrelated"));
    let page = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                text: "absent".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert!(page.entries.is_empty());
    for issue in [
        LogCoverageIssue::GenerationGap,
        LogCoverageIssue::UnsupportedRecord { generation: 2 },
        LogCoverageIssue::InvalidRecord { generation: 2 },
        LogCoverageIssue::TornRecord { generation: 2 },
    ] {
        assert!(page.coverage.issues.contains(&issue));
    }
    assert!(!page.coverage.matches_are_complete);
}

#[test]
fn unindexed_does_not_read_or_create_files_and_cancel_is_explicit() {
    let fixture = Fixture::new(None);
    let page = fixture.read(LogReadQuery::default()).unwrap();
    assert_eq!(page.coverage.status, HistoricalLogStatus::NotIndexed);
    assert!(!fixture.directory.path().join("logs").exists());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        fixture
            .service
            .read_logs(&fixture.scope, LogReadQuery::default(), &cancelled),
        Err(HistoryQueryError::Cancelled)
    ));
    assert!(matches!(
        fixture.service.prepare_log_export(
            &fixture.scope,
            &LogFilter::default(),
            LogExportFormat::Text,
            &cancelled
        ),
        Err(HistoryQueryError::Cancelled)
    ));
    assert!(
        fixture
            .store
            .deployment_log(&fixture.scope.deployment)
            .unwrap()
            .is_none()
    );
}

#[test]
fn exports_complete_safe_matches_across_pages_without_writing() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    fixture.write(1, &event("api", "prepare", "first TOKEN"));
    fixture.write(0, &event("api", "activate", "last\u{1b}\u{200b} TOKEN"));
    for format in [LogExportFormat::Text, LogExportFormat::Json] {
        let prepared = fixture
            .service
            .prepare_log_export(
                &fixture.scope,
                &LogFilter::default(),
                format,
                &CancellationToken::new(),
            )
            .unwrap();
        let text = std::str::from_utf8(prepared.payload()).unwrap();
        assert!(text.contains("first [REDACTED]"));
        assert!(text.contains("last [REDACTED]"));
        assert!(!text.contains("TOKEN"));
        assert!(!text.contains('\u{1b}'));
        assert!(prepared.coverage.matches_are_complete);
        if format == LogExportFormat::Json {
            assert!(serde_json::from_slice::<serde_json::Value>(prepared.payload()).is_ok());
        }
    }
    assert_eq!(
        fs::read_dir(fixture.directory.path().join("logs"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn long_legacy_line_search_matches_across_fragment_boundary_and_invalid_limits_reject() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    let message = format!(
        "{}ABCDE{}",
        "x".repeat(MAX_MESSAGE_BYTES - 2),
        "y".repeat(MAX_MESSAGE_BYTES)
    );
    fixture.write(0, &message);
    let page = fixture
        .read(LogReadQuery {
            filter: LogFilter {
                text: "ABCDE".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        })
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(LogEntry::message)
            .collect::<String>(),
        message
    );
    assert!(matches!(
        fixture.read(LogReadQuery {
            limit: 0,
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::InvalidPage)
    ));
    assert!(matches!(
        fixture.read(LogReadQuery {
            filter: LogFilter {
                text: "x".repeat(129),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::InvalidPage)
    ));
}

#[test]
fn excessive_small_legacy_lines_return_limit_instead_of_no_matches() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::LegacyText));
    fixture.write(0, &"a\n".repeat(MAX_SCAN_ENTRIES + 1));
    assert!(matches!(
        fixture.read(LogReadQuery {
            filter: LogFilter {
                text: "never".into(),
                ..LogFilter::default()
            },
            ..LogReadQuery::default()
        }),
        Err(HistoryQueryError::LogLimit)
    ));
}

#[test]
fn hard_linked_log_is_rejected_before_display_or_export() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let path = fixture.write(0, &event("api", "prepare", "safe"));
    fs::hard_link(path, fixture.directory.path().join("alias.log")).unwrap();
    assert!(matches!(
        fixture.read(LogReadQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    ));
    assert!(matches!(
        fixture.service.prepare_log_export(
            &fixture.scope,
            &LogFilter::default(),
            LogExportFormat::Json,
            &CancellationToken::new()
        ),
        Err(HistoryQueryError::UnsafeLog)
    ));
}

#[cfg(unix)]
#[test]
fn linked_log_directory_is_rejected() {
    let fixture = Fixture::new(Some(DeploymentLogFormat::JsonlV1));
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(other.path(), fixture.directory.path().join("logs")).unwrap();
    assert!(matches!(
        fixture.read(LogReadQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    ));
}
