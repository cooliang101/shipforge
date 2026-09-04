//! Bounded local log navigation. Filtering never creates execution evidence.

use std::{collections::VecDeque, sync::Arc};

use crate::application::history_query::LogFilter;

#[cfg(test)]
use crate::telemetry::log_record::LogScope;
use crate::telemetry::log_record::{
    LogEvent, LogEventKind, MAX_COMMAND_ARGUMENTS, MAX_COMMAND_BYTES, RecordedCommand,
};

const MAX_ROWS: usize = 500;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub(super) struct LogRow {
    /// Stable within this view; not a database row ID or execution authority.
    pub sequence: u64,
    pub elapsed_ms: Option<u64>,
    pub event: Arc<LogEvent>,
}

impl LogRow {
    fn command_within_limits(&self) -> bool {
        match &self.event.kind {
            LogEventKind::FailedCommand { command } => {
                command.args.len() <= MAX_COMMAND_ARGUMENTS
                    && command.program.len().saturating_add(
                        command
                            .args
                            .iter()
                            .map(String::len)
                            .fold(0usize, usize::saturating_add),
                    ) <= MAX_COMMAND_BYTES
            }
            _ => true,
        }
    }

    fn text_bytes(&self) -> usize {
        let scope = self
            .event
            .scope
            .as_ref()
            .map_or(0, |scope| scope.component.as_str().len() + scope.step.len());
        let command = match &self.event.kind {
            LogEventKind::FailedCommand { command } => {
                command.program.len() + command.args.iter().map(String::len).sum::<usize>()
            }
            _ => 0,
        };
        self.event.namespace.len() + self.event.message.len() + scope + command
    }

    fn retained_bytes(&self) -> usize {
        let scope = self.event.scope.as_ref().map_or(0, |scope| {
            scope.component.as_str().len() + scope.step.capacity()
        });
        let command = match &self.event.kind {
            LogEventKind::FailedCommand { command } => {
                command.program.capacity()
                    + command.args.capacity() * size_of::<String>()
                    + command.args.iter().map(String::capacity).sum::<usize>()
            }
            _ => 0,
        };
        size_of::<LogEvent>()
            + 2 * size_of::<usize>()
            + self.event.namespace.capacity()
            + self.event.message.capacity()
            + scope
            + command
    }
}

/// A viewport may retain less than the complete persisted history. The owner
/// must label the source range and load older records through the query service.
#[derive(Clone, Debug)]
pub(super) struct LogView {
    rows: VecDeque<LogRow>,
    bytes: usize,
    selected: Option<u64>,
    follow: bool,
    filter: LogFilter,
    omitted: u64,
    selection_evicted: bool,
    last_seen_sequence: Option<u64>,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            rows: VecDeque::new(),
            bytes: 0,
            selected: None,
            follow: true,
            filter: LogFilter::default(),
            omitted: 0,
            selection_evicted: false,
            last_seen_sequence: None,
        }
    }
}

impl LogView {
    /// Refuses oversized or non-monotonic rows instead of silently truncating
    /// a failed command and presenting it as the complete recorded argv.
    pub fn push(&mut self, mut row: LogRow) -> bool {
        if self
            .last_seen_sequence
            .is_some_and(|last| row.sequence <= last)
        {
            return false;
        }
        self.last_seen_sequence = Some(row.sequence);
        if !row.command_within_limits() || row.text_bytes() > MAX_ROW_BYTES {
            self.omitted = self.omitted.saturating_add(1);
            return false;
        }
        // Clone strings/argv into compact owned storage: an upstream String
        // with tiny length but huge reserved capacity must not inflate this view.
        row.event = Arc::new(row.event.as_ref().clone());
        let bytes = row.retained_bytes();
        self.bytes += bytes;
        self.rows.push_back(row);
        while self.rows.len() > MAX_ROWS
            || self.bytes + self.rows.capacity() * size_of::<LogRow>() > MAX_BYTES
        {
            if let Some(removed) = self.rows.pop_front() {
                self.bytes -= removed.retained_bytes();
                self.omitted = self.omitted.saturating_add(1);
                if self.selected == Some(removed.sequence) && !self.follow {
                    self.selection_evicted = true;
                }
            }
        }
        self.reconcile_selection();
        true
    }

    pub fn set_filter(&mut self, filter: &LogFilter) -> bool {
        let query = &filter.text;
        if query.len() > MAX_QUERY_BYTES
            || query.chars().count() > 128
            || query.chars().any(char::is_control)
            || filter.step.as_ref().is_some_and(|step| {
                step.is_empty() || step.len() > 256 || step.chars().any(char::is_control)
            })
        {
            return false;
        }
        let query = query.to_lowercase();
        if query.len() > MAX_QUERY_BYTES {
            return false;
        }
        self.filter = filter.clone();
        self.filter.text = query;
        self.follow = false;
        self.selected = None;
        self.reconcile_selection();
        true
    }

    #[cfg(test)]
    fn set_query(&mut self, query: &str) -> bool {
        let mut filter = self.filter.clone();
        filter.text = query.into();
        self.set_filter(&filter)
    }

    #[cfg(test)]
    fn set_scope(&mut self, scope: Option<LogScope>) {
        let mut filter = self.filter.clone();
        filter.component = scope.as_ref().map(|scope| scope.component.clone());
        filter.step = scope.map(|scope| scope.step);
        assert!(self.set_filter(&filter));
    }

    pub fn follow_latest(&mut self) {
        self.follow = true;
        self.selection_evicted = false;
        self.reconcile_selection();
    }

    pub fn first(&mut self) {
        self.follow = false;
        self.selection_evicted = false;
        self.selected = self.matching().first().map(|row| row.sequence);
    }

    pub fn move_selection(&mut self, forward: bool, amount: usize) {
        self.follow = false;
        self.selection_evicted = false;
        let rows = self.matching();
        let selected = rows
            .iter()
            .position(|row| Some(row.sequence) == self.selected)
            .unwrap_or(0);
        let next = if forward {
            selected
                .saturating_add(amount)
                .min(rows.len().saturating_sub(1))
        } else {
            selected.saturating_sub(amount)
        };
        self.selected = rows.get(next).map(|row| row.sequence);
    }

    pub fn matching(&self) -> Vec<&LogRow> {
        self.rows.iter().filter(|row| self.matches(row)).collect()
    }

    pub fn selected(&self) -> Option<&LogRow> {
        self.rows
            .iter()
            .find(|row| Some(row.sequence) == self.selected)
    }

    pub fn failed_command(&self) -> Option<&RecordedCommand> {
        match &self.selected()?.event.kind {
            LogEventKind::FailedCommand { command } => Some(command),
            _ => None,
        }
    }

    pub const fn is_following(&self) -> bool {
        self.follow
    }

    pub const fn omitted(&self) -> u64 {
        self.omitted
    }

    pub const fn selection_evicted(&self) -> bool {
        self.selection_evicted
    }

    pub fn query(&self) -> &str {
        &self.filter.text
    }

    #[cfg(test)]
    fn scope(&self) -> Option<LogScope> {
        Some(LogScope {
            component: self.filter.component.clone()?,
            step: self.filter.step.clone()?,
        })
    }

    fn matches(&self, row: &LogRow) -> bool {
        let scope = row.event.scope.as_ref();
        if self
            .filter
            .component
            .as_ref()
            .is_some_and(|component| scope.is_none_or(|scope| &scope.component != component))
            || self
                .filter
                .step
                .as_ref()
                .is_some_and(|step| scope.is_none_or(|scope| &scope.step != step))
        {
            return false;
        }
        let contains = |value: &str| {
            self.filter.text.is_empty() || value.to_lowercase().contains(&self.filter.text)
        };
        contains(&row.event.message)
            || contains(&row.event.namespace)
            || scope
                .is_some_and(|scope| contains(scope.component.as_str()) || contains(&scope.step))
            || match &row.event.kind {
                LogEventKind::FailedCommand { command } => {
                    contains(&command.program)
                        || command.args.iter().any(|argument| contains(argument))
                }
                _ => false,
            }
    }

    fn reconcile_selection(&mut self) {
        let rows = self.matching();
        self.selected = if self.follow {
            rows.last().map(|row| row.sequence)
        } else if rows.iter().any(|row| Some(row.sequence) == self.selected) {
            self.selected
        } else {
            rows.first().map(|row| row.sequence)
        };
    }
}

#[cfg(test)]
mod tests;
