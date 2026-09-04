use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::OrchestrationError;

#[derive(Debug, Default)]
pub(super) struct MonotonicClock {
    last_timestamp_ms: AtomicU64,
}

impl MonotonicClock {
    pub(super) fn timestamp(&self) -> Result<u64, OrchestrationError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| OrchestrationError::Clock(error.to_string()))?;
        let wall_clock = u64::try_from(duration.as_millis())
            .map_err(|error| OrchestrationError::Clock(error.to_string()))?;
        let previous = self
            .last_timestamp_ms
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
                Some(wall_clock.max(previous.saturating_add(1)))
            })
            .unwrap_or(wall_clock);
        Ok(wall_clock.max(previous.saturating_add(1)))
    }
}
