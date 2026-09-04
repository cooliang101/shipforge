use super::*;
use crate::{domain::ComponentName, telemetry::log_record::CommandLocation};

fn scope(component: &str, step: &str) -> LogScope {
    LogScope {
        component: ComponentName::parse(component).unwrap(),
        step: step.into(),
    }
}

fn row(sequence: u64, message: &str, scope: Option<LogScope>) -> LogRow {
    LogRow {
        sequence,
        elapsed_ms: Some(sequence),
        event: Arc::new(LogEvent {
            namespace: "build.stdout".into(),
            message: message.into(),
            scope,
            kind: LogEventKind::Output,
        }),
    }
}

#[test]
fn following_and_paused_navigation_preserve_stable_selection() {
    let mut view = LogView::default();
    for index in 1..=5 {
        assert!(view.push(row(index, "output", None)));
    }
    assert!(view.is_following());
    assert_eq!(view.selected().unwrap().sequence, 5);
    assert_eq!(view.selected().unwrap().elapsed_ms, Some(5));
    view.move_selection(false, 2);
    assert!(!view.is_following());
    view.push(row(6, "new output", None));
    assert_eq!(view.selected().unwrap().sequence, 3);
    view.follow_latest();
    assert_eq!(view.selected().unwrap().sequence, 6);
    view.first();
    assert_eq!(view.selected().unwrap().sequence, 1);
}

#[test]
fn filtering_uses_real_component_and_step_not_message_claims() {
    let mut view = LogView::default();
    view.push(row(1, "ERROR from backend / prepare", None));
    view.push(row(
        2,
        "Error reading file",
        Some(scope("backend", "prepare")),
    ));
    view.push(row(
        3,
        "error reading file",
        Some(scope("worker", "prepare")),
    ));
    view.push(row(
        4,
        "error reading file",
        Some(scope("backend", "activate")),
    ));
    assert!(view.set_query("eRrOr"));
    assert_eq!(view.query(), "error");
    view.set_scope(Some(scope("backend", "prepare")));
    assert_eq!(view.scope(), Some(scope("backend", "prepare")));
    assert_eq!(view.matching().len(), 1);
    assert_eq!(view.selected().unwrap().sequence, 2);
    view.set_scope(None);
    assert_eq!(view.matching().len(), 4);
    assert!(view.set_query("不存在"));
    assert!(view.selected().is_none());
    view.move_selection(true, usize::MAX);
    assert!(view.selected().is_none());
}

#[test]
fn independent_component_and_step_filters_preserve_live_follow_updates() {
    let mut view = LogView::default();
    view.push(row(1, "data", Some(scope("backend", "prepare"))));
    view.push(row(2, "data", Some(scope("backend", "activate"))));
    view.push(row(3, "data", Some(scope("worker", "prepare"))));
    let mut filter = LogFilter {
        component: Some(ComponentName::parse("backend").unwrap()),
        ..LogFilter::default()
    };
    assert!(view.set_filter(&filter));
    assert_eq!(view.matching().len(), 2);
    filter.component = None;
    filter.step = Some("prepare".into());
    assert!(view.set_filter(&filter));
    assert_eq!(view.matching().len(), 2);
    view.follow_latest();
    view.push(row(4, "new prepare", Some(scope("worker", "prepare"))));
    assert_eq!(view.selected().unwrap().sequence, 4);
    view.push(row(5, "new activate", Some(scope("worker", "activate"))));
    assert_eq!(view.selected().unwrap().sequence, 4);
}

#[test]
fn live_text_filter_matches_recorded_argv_without_inferring_a_command_from_output() {
    let mut view = LogView::default();
    let mut event = row(1, "failed", Some(scope("backend", "build-package")));
    Arc::make_mut(&mut event.event).kind = LogEventKind::FailedCommand {
        command: RecordedCommand {
            location: CommandLocation::Local,
            index: Some(2),
            program: "builder".into(),
            args: vec!["--STATIC-ASSET".into()],
        },
    };
    view.push(event);
    assert!(view.set_filter(&LogFilter {
        text: "static-asset".into(),
        ..LogFilter::default()
    }));
    assert_eq!(view.matching().len(), 1);
    assert_eq!(view.failed_command().unwrap().program, "builder");
}

#[test]
fn evicted_paused_selection_is_explicit_and_window_stays_bounded() {
    let mut view = LogView::default();
    view.push(row(1, "first", None));
    view.first();
    for index in 2..=502 {
        view.push(row(index, "next", None));
    }
    assert_eq!(view.rows.len(), MAX_ROWS);
    assert_eq!(view.omitted(), 2);
    assert!(view.selection_evicted());
    assert_eq!(view.selected().unwrap().sequence, 3);
    assert!(!view.is_following());
    view.follow_latest();
    assert!(!view.selection_evicted());
    assert_eq!(view.selected().unwrap().sequence, 502);
}

#[test]
fn byte_limit_and_oversized_rows_do_not_silently_truncate_commands() {
    let mut view = LogView::default();
    let message = "文".repeat(10_000);
    for index in 0..200 {
        view.push(row(index, &message, None));
    }
    assert!(view.bytes <= MAX_BYTES);
    assert!(view.rows.len() < MAX_ROWS);
    assert!(view.omitted() > 0);
    let mut oversized = row(201, "command failed", None);
    Arc::make_mut(&mut oversized.event).kind = LogEventKind::FailedCommand {
        command: RecordedCommand {
            location: CommandLocation::Local,
            index: Some(1),
            program: "builder".into(),
            args: vec!["x".repeat(MAX_ROW_BYTES)],
        },
    };
    assert!(!view.push(oversized));
    assert!(view.failed_command().is_none());
}

#[test]
fn command_copy_source_is_only_the_selected_recorded_snapshot() {
    let mut view = LogView::default();
    view.push(row(1, "try running old-command --secret value", None));
    assert!(view.failed_command().is_none());
    let command = RecordedCommand {
        location: CommandLocation::Local,
        index: Some(2),
        program: "builder".into(),
        args: vec!["argument with spaces".into(), "[REDACTED]".into()],
    };
    let mut failed = row(
        2,
        "build command failed",
        Some(scope("backend", "build-package")),
    );
    Arc::make_mut(&mut failed.event).kind = LogEventKind::FailedCommand {
        command: command.clone(),
    };
    view.push(failed);
    assert_eq!(view.failed_command(), Some(&command));
    view.first();
    assert!(view.failed_command().is_none());
}

#[test]
fn invalid_queries_and_stale_rows_preserve_current_view() {
    let mut view = LogView::default();
    view.push(row(1, "match", None));
    assert!(view.set_query("match"));
    assert!(!view.set_query(&"x".repeat(MAX_QUERY_BYTES + 1)));
    assert!(!view.set_query("\u{1b}[31m"));
    assert_eq!(view.query(), "match");
    assert!(!view.push(row(1, "stale replacement", None)));
    assert_eq!(view.selected().unwrap().event.message, "match");
    assert_eq!(
        view.omitted(),
        0,
        "duplicate delivery does not lose a log row"
    );
    assert!(!view.set_query(&"İ".repeat(256)));
    assert_eq!(view.query(), "match");
}

#[test]
fn empty_arguments_and_excess_string_capacity_cannot_bypass_the_budget() {
    let mut view = LogView::default();
    let mut oversized = row(1, "failed", None);
    Arc::make_mut(&mut oversized.event).kind = LogEventKind::FailedCommand {
        command: RecordedCommand {
            location: CommandLocation::Local,
            index: Some(1),
            program: "builder".into(),
            args: vec![String::new(); MAX_COMMAND_ARGUMENTS + 1],
        },
    };
    assert!(!view.push(oversized));
    assert!(view.rows.is_empty());
    assert_eq!(view.omitted(), 1);
    let mut small = row(2, "small", None);
    Arc::make_mut(&mut small.event)
        .message
        .reserve(MAX_BYTES * 2);
    assert!(small.event.message.capacity() > MAX_BYTES);
    assert!(view.push(small));
    assert!(view.bytes < MAX_ROW_BYTES);
    assert_eq!(view.selected().unwrap().event.message, "small");
    assert!(!view.push(row(1, "stale", None)));
    assert_eq!(view.omitted(), 1);
}
