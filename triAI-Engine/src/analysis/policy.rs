//! Deterministic, versioned policy proposals from aggregate measurements.

use super::job::AggregateWindow;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyProposal {
    pub version: u64,
    pub source_window_start_ms: u64,
    pub source_window_end_ms: u64,
    pub cache_budget_mb: u32,
    pub prefetch_depth: u32,
    pub confidence: f64,
    pub disk_savings_percent: f64,
    pub decode_change_percent: f64,
}

#[derive(Debug, Default)]
pub struct PolicyVersioner {
    last_version: u64,
}

impl PolicyVersioner {
    pub fn next(&mut self, window: AggregateWindow) -> PolicyProposal {
        self.last_version = self.last_version.saturating_add(1);
        let decode_change_percent = if window.baseline_decode_p95_ms == 0 {
            0.0
        } else {
            ((window.candidate_decode_p95_ms as f64 / window.baseline_decode_p95_ms as f64) - 1.0)
                * 100.0
        };
        PolicyProposal {
            version: self.last_version,
            source_window_start_ms: window.start_ms,
            source_window_end_ms: window.end_ms,
            // The proposal remains conservative: only bounded numeric knobs.
            cache_budget_mb: if window.disk_savings_percent >= 15.0 {
                512
            } else {
                256
            },
            prefetch_depth: if decode_change_percent <= 0.0 { 2 } else { 1 },
            confidence: window.confidence,
            disk_savings_percent: window.disk_savings_percent,
            decode_change_percent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_monotonic() {
        let window = AggregateWindow {
            start_ms: 1,
            end_ms: 2,
            observations: 30,
            independent_model_observations: 2,
            disk_savings_percent: 20.0,
            baseline_decode_p95_ms: 100,
            candidate_decode_p95_ms: 100,
            confidence: 0.9,
        };
        let mut versioner = PolicyVersioner::default();
        assert_eq!(versioner.next(window).version, 1);
        assert_eq!(versioner.next(window).version, 2);
    }
}
