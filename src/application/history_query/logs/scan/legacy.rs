use super::super::LogEntryContent;
use super::{
    CancellationToken, HistoryQueryError, LogCoverageIssue, LogFilter, LogScan, MAX_SCAN_BYTES,
    MAX_SCAN_ENTRIES, check_cancelled, contains_text,
};
use crate::telemetry::{
    Redactor,
    log_record::{MAX_MESSAGE_BYTES, sanitize_log_text},
};

const PRIVATE_LABELS: [&str; 7] = [
    "PRIVATE KEY",
    "ENCRYPTED PRIVATE KEY",
    "RSA PRIVATE KEY",
    "EC PRIVATE KEY",
    "DSA PRIVATE KEY",
    "OPENSSH PRIVATE KEY",
    "PGP PRIVATE KEY BLOCK",
];
const HIDDEN_BODY: &str =
    "Legacy run body unavailable: retained boundaries do not establish safe complete context\n";

#[derive(Default)]
pub(super) struct LegacyRuns {
    runs: Vec<Vec<(u32, Vec<u8>)>>,
}

impl LegacyRuns {
    pub(super) fn push(&mut self, generation: u32, bytes: Vec<u8>) {
        if let Some(run) = self.runs.last_mut()
            && run
                .last()
                .is_some_and(|(previous, _)| *previous == generation + 1)
        {
            run.push((generation, bytes));
        } else {
            self.runs.push(vec![(generation, bytes)]);
        }
    }

    pub(super) fn project(
        self,
        scan: &mut LogScan,
        redactor: &Redactor,
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<(), HistoryQueryError> {
        for (index, files) in self.runs.into_iter().enumerate() {
            check_cancelled(cancellation)?;
            let generations: Vec<_> = files.iter().map(|(generation, _)| *generation).collect();
            let mut raw = Vec::new();
            let mut safe_parts = Vec::new();
            let mut safe_bytes = 0_usize;
            for (generation, bytes) in files {
                check_cancelled(cancellation)?;
                if raw.len().saturating_add(bytes.len()) > MAX_SCAN_BYTES {
                    return Err(HistoryQueryError::LogLimit);
                }
                let safe =
                    sanitize_log_text(&String::from_utf8_lossy(&bytes), redactor, MAX_SCAN_BYTES)
                        .map_err(|_| HistoryQueryError::LogLimit)?;
                safe_bytes = safe_bytes.saturating_add(safe.len());
                if safe_bytes > MAX_SCAN_BYTES {
                    return Err(HistoryQueryError::LogLimit);
                }
                safe_parts.push((generation, safe));
                raw.extend(bytes);
            }
            let text = String::from_utf8_lossy(&raw);
            if text.len() > MAX_SCAN_BYTES {
                return Err(HistoryQueryError::LogLimit);
            }
            let uncertain = index > 0 || orphan_end(&text, cancellation)?;
            let safe = if uncertain {
                scan.issue(LogCoverageIssue::IncompleteLegacy {
                    generation: generations[0],
                });
                HIDDEN_BODY.into()
            } else {
                sanitize_log_text(&text, redactor, MAX_SCAN_BYTES)
                    .map_err(|_| HistoryQueryError::LogLimit)?
            };
            let independent = !uncertain
                && safe
                    .bytes()
                    .eq(safe_parts.iter().flat_map(|(_, text)| text.bytes()));
            scan.legacy_independent &= independent;
            if independent {
                scan.legacy_pages.extend(safe_parts);
            }
            project_lines(scan, &generations, &safe, filter, cancellation)?;
        }
        Ok(())
    }
}

fn orphan_end(text: &str, cancellation: &CancellationToken) -> Result<bool, HistoryQueryError> {
    let normalized: String = text.chars().filter(|character| (!character.is_control() || matches!(character, '\n' | '\t')) && !matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')).collect();
    let markers: Vec<_> = PRIVATE_LABELS
        .iter()
        .map(|label| {
            (
                format!("-----BEGIN {label}-----"),
                format!("-----END {label}-----"),
            )
        })
        .collect();
    // Match complete markers: non-overlapping searches for only "-----" miss
    // valid markers preceded by an extra hyphen ("------END ...").
    let mut positions = Vec::new();
    for (label, (begin, end)) in markers.iter().enumerate() {
        for (marker, is_begin) in [(begin, true), (end, false)] {
            check_cancelled(cancellation)?;
            for (position, _) in normalized.match_indices(marker.as_str()) {
                check_cancelled(cancellation)?;
                if positions.len() >= MAX_SCAN_ENTRIES {
                    return Err(HistoryQueryError::LogLimit);
                }
                positions.push((position, label, is_begin));
            }
        }
    }
    positions.sort_unstable();
    let mut opened = None;
    for (_, label, is_begin) in positions {
        check_cancelled(cancellation)?;
        if is_begin {
            if opened.is_some() {
                return Ok(true);
            }
            opened = Some(label);
        } else {
            if opened != Some(label) {
                return Ok(true);
            }
            opened = None;
        }
    }
    Ok(false)
}

fn project_lines(
    scan: &mut LogScan,
    generations: &[u32],
    text: &str,
    filter: &LogFilter,
    cancellation: &CancellationToken,
) -> Result<(), HistoryQueryError> {
    for line in text.split_inclusive('\n') {
        check_cancelled(cancellation)?;
        let matches = filter.component.is_none()
            && filter.step.is_none()
            && contains_text(line, &filter.text);
        let mut remaining = line;
        while !remaining.is_empty() {
            check_cancelled(cancellation)?;
            let mut end = remaining.len().min(MAX_MESSAGE_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            scan.push(
                generations[0],
                generations.to_vec(),
                LogEntryContent::Legacy(remaining[..end].into()),
                matches,
            )?;
            remaining = &remaining[end..];
        }
    }
    Ok(())
}
