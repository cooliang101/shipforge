use super::super::LogEntryContent;
use super::{
    CancellationToken, HistoryQueryError, LogCoverageIssue, LogEventKind, LogFilter, LogRecord,
    LogScan, MAX_SCAN_BYTES, check_cancelled, record_matches,
};
use crate::telemetry::{
    Redactor,
    log_record::{LogFragment, MAX_MESSAGE_BYTES, sanitize_log_event},
};

/// Never leaves this reader's private assembly state with an unprojected body.
pub(super) struct RawEntry {
    pub(super) generation: u32,
    pub(super) record: LogRecord,
}

impl LogScan {
    pub(super) fn receive_record(
        &mut self,
        entry: RawEntry,
        redactor: &Redactor,
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<(), HistoryQueryError> {
        let Some(fragment) = entry.record.fragment else {
            self.finish_fragments(redactor, filter, cancellation)?;
            self.pending.push(entry);
            return self.project_complete_group(redactor, filter, cancellation);
        };
        if fragment.index == 0 {
            self.finish_fragments(redactor, filter, cancellation)?;
            self.pending.push(entry);
            return Ok(());
        }
        let valid = self.pending.first().is_some_and(|first| {
            first.record.fragment.is_some_and(|initial| {
                initial.event_id == fragment.event_id
                    && initial.count == fragment.count
                    && fragment.index as usize == self.pending.len()
            }) && first.record.event.scope == entry.record.event.scope
                && first.record.event.namespace == entry.record.event.namespace
                && first.record.elapsed_ms == entry.record.elapsed_ms
        });
        if !valid {
            self.finish_fragments(redactor, filter, cancellation)?;
            return self.hide_incomplete(entry, redactor, filter);
        }
        self.pending.push(entry);
        if fragment.index + 1 == fragment.count {
            self.project_complete_group(redactor, filter, cancellation)?;
        }
        Ok(())
    }

    pub(super) fn finish_fragments(
        &mut self,
        redactor: &Redactor,
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<(), HistoryQueryError> {
        for entry in std::mem::take(&mut self.pending) {
            check_cancelled(cancellation)?;
            self.hide_incomplete(entry, redactor, filter)?;
        }
        Ok(())
    }

    fn hide_incomplete(
        &mut self,
        mut entry: RawEntry,
        redactor: &Redactor,
        filter: &LogFilter,
    ) -> Result<(), HistoryQueryError> {
        self.issue(LogCoverageIssue::IncompleteFragments {
            generation: entry.generation,
        });
        entry.record.event.message =
            "Fragment body unavailable: complete event evidence is missing".into();
        entry.record.event = sanitize_log_event(&entry.record.event, redactor)
            .map_err(|_| HistoryQueryError::LogLimit)?;
        let matches = record_matches(&entry.record, filter);
        self.push(
            entry.generation,
            vec![entry.generation],
            LogEntryContent::Structured(entry.record),
            matches,
        )
    }

    fn project_complete_group(
        &mut self,
        redactor: &Redactor,
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<(), HistoryQueryError> {
        let originals = std::mem::take(&mut self.pending);
        let Some(first) = originals.first() else {
            return Ok(());
        };
        let mut combined = first.record.clone();
        combined.event.message.clear();
        for entry in &originals {
            check_cancelled(cancellation)?;
            if combined
                .event
                .message
                .len()
                .saturating_add(entry.record.event.message.len())
                > MAX_SCAN_BYTES
            {
                return Err(HistoryQueryError::LogLimit);
            }
            combined.event.message.push_str(&entry.record.event.message);
        }
        // PEM markers and registered secrets survive until all explicitly
        // connected raw bodies are together; per-fragment sanitation loses context.
        combined.event = sanitize_log_event(&combined.event, redactor)
            .map_err(|_| HistoryQueryError::LogLimit)?;
        if combined.event.scope.is_none() && (filter.component.is_some() || filter.step.is_some()) {
            self.issue(LogCoverageIssue::UnscopedRecords {
                generation: first.generation,
            });
        }
        let matches = record_matches(&combined, filter);
        let safe = std::mem::take(&mut combined.event.message);
        let pieces = utf8_fragments(&safe);
        let count = u32::try_from(pieces.len()).map_err(|_| HistoryQueryError::LogLimit)?;
        let mut source_generations = Vec::new();
        for source in &originals {
            if !source_generations.contains(&source.generation) {
                source_generations.push(source.generation);
            }
        }
        for (index, piece) in pieces.into_iter().enumerate() {
            check_cancelled(cancellation)?;
            // These are safe view fragments, not a rewrite of original file records.
            let mut record = combined.clone();
            record.event.message = piece.into();
            if index > 0 {
                record.event.kind = LogEventKind::Output;
            }
            record.fragment =
                first
                    .record
                    .fragment
                    .filter(|_| count > 1)
                    .map(|fragment| LogFragment {
                        event_id: fragment.event_id,
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                        count,
                    });
            self.push(
                first.generation,
                source_generations.clone(),
                LogEntryContent::Structured(record),
                matches,
            )?;
        }
        Ok(())
    }
}

fn utf8_fragments(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut remaining = text;
    let mut pieces = Vec::new();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(MAX_MESSAGE_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&remaining[..end]);
        remaining = &remaining[end..];
    }
    pieces
}
