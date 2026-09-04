//! Bounded live projection; neither dropped output nor UI polling owns execution.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use crate::{
    domain::{ComponentName, DeploymentId},
    telemetry::{
        Redactor,
        log_record::{
            LogEvent, LogEventKind, LogPersistence, LogScope, LogStepState, MAX_COMMAND_ARGUMENTS,
            MAX_COMMAND_BYTES, MAX_MESSAGE_BYTES, split_log_event,
        },
    },
};

use super::log_view::LogRow;

const MAX_ROWS: usize = 128;
const MAX_ROW_BYTES: usize = 1024 * 1024;
const MAX_STEPS: usize = 1024;

#[derive(Clone, Debug)]
pub(super) struct LiveProgress {
    inner: Arc<Mutex<State>>,
    started: Instant,
    redactor: Redactor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LiveStep {
    pub scope: LogScope,
    pub state: LogStepState,
    pub persistence: LogPersistence,
    pub started_elapsed_ms: Option<u64>,
    pub finished_elapsed_ms: Option<u64>,
    pub updated_sequence: u64,
}

#[derive(Clone, Debug)]
pub(super) struct LiveSnapshot {
    pub deployment: Option<DeploymentId>,
    pub elapsed_ms: u64,
    pub finished: bool,
    pub steps: Vec<LiveStep>,
    pub dropped_rows: u64,
    pub dropped_steps: u64,
    pub rejected_events: u64,
    pub late_events: u64,
    pub poisoned: bool,
}

#[derive(Debug)]
pub(super) struct LiveUpdate {
    pub rows: Vec<LogRow>,
    pub snapshot: LiveSnapshot,
}

#[derive(Debug, Default)]
struct State {
    deployment: Option<DeploymentId>,
    rows: VecDeque<LogRow>,
    row_bytes: usize,
    steps: BTreeMap<(ComponentName, String), LiveStep>,
    next_sequence: u64,
    elapsed_ms: u64,
    finished: bool,
    dropped_rows: u64,
    dropped_steps: u64,
    rejected_events: u64,
    late_events: u64,
    poisoned: bool,
}

impl Default for LiveProgress {
    fn default() -> Self {
        Self::new(Redactor::default())
    }
}

impl LiveProgress {
    pub fn new(redactor: Redactor) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
            started: Instant::now(),
            redactor,
        }
    }

    /// Called by the event sink, not by the redraw loop. Sanitizing and splitting
    /// happen outside the lock; there is no channel send or wait for UI draining.
    pub fn record(&self, event: LogEvent) {
        self.record_at(event, self.started.elapsed());
    }

    fn record_at(&self, mut event: LogEvent, elapsed: Duration) {
        // The writer normally supplies <=16 KiB fragments. Defend this API too:
        // never clone an arbitrarily large raw message/argv just to reject it.
        let omitted = bound_input(&mut event);
        let events = split_log_event(&event, &self.redactor);
        let mut state = self.lock();
        if state.finished {
            state.late_events = state.late_events.saturating_add(1);
            return;
        }
        state.advance(elapsed);
        let Ok(events) = events else {
            state.rejected_events = state.rejected_events.saturating_add(1);
            return;
        };
        if omitted {
            state.rejected_events = state.rejected_events.saturating_add(1);
        }
        for event in events {
            state.record(event);
        }
    }

    pub fn snapshot(&self) -> LiveSnapshot {
        let mut state = self.lock();
        state.advance(self.started.elapsed());
        state.snapshot()
    }

    /// Identifies one worker projection even before it supplies a Deployment ID.
    pub fn same_operation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Takes only the pending bounded output window. Step facts and cumulative
    /// omission counters survive draining, including when no row can be retained.
    pub fn drain(&self) -> LiveUpdate {
        let mut state = self.lock();
        state.advance(self.started.elapsed());
        let rows = state.rows.drain(..).collect();
        state.row_bytes = 0;
        LiveUpdate {
            rows,
            snapshot: state.snapshot(),
        }
    }

    /// Call only after the tracked worker and log writer have terminated.
    /// This freezes the operation clock, not an inferred remote outcome.
    pub fn finish(&self) {
        self.finish_at(self.started.elapsed());
    }

    fn finish_at(&self, elapsed: Duration) {
        let mut state = self.lock();
        state.advance(elapsed);
        state.finished = true;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.poisoned = true;
            state
        })
    }
}

impl State {
    fn advance(&mut self, elapsed: Duration) {
        if !self.finished {
            self.elapsed_ms = self
                .elapsed_ms
                .max(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        }
    }

    fn record(&mut self, event: LogEvent) {
        if let LogEventKind::DeploymentStarted { deployment } = &event.kind {
            if self
                .deployment
                .as_ref()
                .is_some_and(|known| known != deployment)
            {
                self.rejected_events = self.rejected_events.saturating_add(1);
                return;
            }
            self.deployment = Some(deployment.clone());
        }
        let Some(sequence) = self.next_sequence.checked_add(1) else {
            self.rejected_events = self.rejected_events.saturating_add(1);
            return;
        };
        self.next_sequence = sequence;
        if let (Some(scope), LogEventKind::Step { state, persistence }) =
            (&event.scope, &event.kind)
        {
            self.update_step(scope, *state, *persistence, sequence);
        }
        let row = LogRow {
            sequence,
            elapsed_ms: Some(self.elapsed_ms),
            event: Arc::new(event),
        };
        self.row_bytes += retained_bytes(&row);
        self.rows.push_back(row);
        while self.rows.len() > MAX_ROWS
            || self.row_bytes + self.rows.capacity() * size_of::<LogRow>() > MAX_ROW_BYTES
        {
            let Some(removed) = self.rows.pop_front() else {
                break;
            };
            self.row_bytes -= retained_bytes(&removed);
            self.dropped_rows = self.dropped_rows.saturating_add(1);
        }
    }

    fn update_step(
        &mut self,
        scope: &LogScope,
        state: LogStepState,
        persistence: LogPersistence,
        sequence: u64,
    ) {
        let key = (scope.component.clone(), scope.step.clone());
        if !self.steps.contains_key(&key)
            && self.steps.len() == MAX_STEPS
            && let Some(oldest) = self
                .steps
                .iter()
                .min_by_key(|(_, step)| step.updated_sequence)
                .map(|(key, _)| key.clone())
        {
            self.steps.remove(&oldest);
            self.dropped_steps = self.dropped_steps.saturating_add(1);
        }
        let step = self.steps.entry(key).or_insert_with(|| LiveStep {
            scope: scope.clone(),
            state,
            persistence,
            started_elapsed_ms: None,
            finished_elapsed_ms: None,
            updated_sequence: sequence,
        });
        step.state = state;
        step.persistence = persistence;
        step.updated_sequence = sequence;
        if state == LogStepState::Started {
            step.started_elapsed_ms.get_or_insert(self.elapsed_ms);
        } else {
            step.finished_elapsed_ms = Some(self.elapsed_ms);
        }
    }

    fn snapshot(&self) -> LiveSnapshot {
        LiveSnapshot {
            deployment: self.deployment.clone(),
            elapsed_ms: self.elapsed_ms,
            finished: self.finished,
            steps: self.steps.values().cloned().collect(),
            dropped_rows: self.dropped_rows,
            dropped_steps: self.dropped_steps,
            rejected_events: self.rejected_events,
            late_events: self.late_events,
            poisoned: self.poisoned,
        }
    }
}

fn retained_bytes(row: &LogRow) -> usize {
    let event = &row.event;
    let scope = event.scope.as_ref().map_or(0, |scope| {
        scope.component.as_str().len() + scope.step.capacity()
    });
    let command = match &event.kind {
        LogEventKind::FailedCommand { command } => {
            command.program.capacity()
                + command.args.capacity() * size_of::<String>()
                + command.args.iter().map(String::capacity).sum::<usize>()
        }
        _ => 0,
    };
    size_of::<LogEvent>()
        + 2 * size_of::<usize>()
        + event.namespace.capacity()
        + event.message.capacity()
        + scope
        + command
}

fn bound_input(event: &mut LogEvent) -> bool {
    let mut omitted = false;
    if event.message.len() > 4 * MAX_MESSAGE_BYTES {
        event.message = "Oversized live output omitted; consult the persisted log".into();
        omitted = true;
    }
    if let LogEventKind::FailedCommand { command } = &event.kind {
        let bytes = command
            .args
            .iter()
            .take(MAX_COMMAND_ARGUMENTS + 1)
            .fold(command.program.len(), |bytes, value| {
                bytes.saturating_add(value.len())
            });
        if command.args.len() > MAX_COMMAND_ARGUMENTS || bytes > MAX_COMMAND_BYTES {
            event.kind = LogEventKind::CommandUnavailable {
                location: command.location,
                index: command.index.filter(|index| *index > 0),
            };
            omitted = true;
        }
    }
    // Invalid labels cannot establish a bounded step identity. Replace no scope
    // with invented data: the codec rejects it and the snapshot reports a gap.
    if event.namespace.len() > 256 {
        event.namespace = String::new();
        omitted = true;
    }
    if let Some(scope) = &mut event.scope
        && scope.step.len() > 256
    {
        scope.step = String::new();
        omitted = true;
    }
    omitted
}

#[cfg(test)]
mod tests;
