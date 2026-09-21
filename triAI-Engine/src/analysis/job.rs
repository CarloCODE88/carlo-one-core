//! Scheduling primitives for analysis jobs using aggregate evidence only.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// Metrics collected over a closed time window.  There are intentionally no
/// prompt, source-code, completion, user, or free-form text fields.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AggregateWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub observations: u64,
    pub independent_model_observations: u32,
    pub disk_savings_percent: f64,
    pub baseline_decode_p95_ms: u64,
    pub candidate_decode_p95_ms: u64,
    pub confidence: f64,
}

impl AggregateWindow {
    pub fn is_valid(&self) -> bool {
        self.end_ms > self.start_ms
            && self.observations > 0
            && self.disk_savings_percent.is_finite()
            && self.confidence.is_finite()
            && (0.0..=1.0).contains(&self.confidence)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisKind {
    TransferBottleneck,
    MoeHotset,
    ModelCalibration,
    AnomalyDetection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisJob {
    pub kind: AnalysisKind,
    pub window: AggregateWindow,
}

impl AnalysisJob {
    /// A stable idempotency key: resubmitting the same kind/window is a no-op.
    pub fn idempotency_key(&self) -> (AnalysisKind, u64, u64) {
        (self.kind, self.window.start_ms, self.window.end_ms)
    }
}

/// Non-blocking scheduler state.  A worker can poll `take_due`; the scheduler
/// itself never runs model inference or touches raw evidence.
#[derive(Debug)]
pub struct AnalysisScheduler {
    intervals: BTreeMap<AnalysisKind, Duration>,
    last_run_ms: BTreeMap<AnalysisKind, u64>,
    submitted: HashSet<(AnalysisKind, u64, u64)>,
}

impl Default for AnalysisScheduler {
    fn default() -> Self {
        let mut intervals = BTreeMap::new();
        intervals.insert(AnalysisKind::TransferBottleneck, Duration::from_secs(60));
        intervals.insert(AnalysisKind::MoeHotset, Duration::from_secs(300));
        intervals.insert(AnalysisKind::ModelCalibration, Duration::from_secs(900));
        intervals.insert(AnalysisKind::AnomalyDetection, Duration::from_secs(60));
        Self {
            intervals,
            last_run_ms: BTreeMap::new(),
            submitted: HashSet::new(),
        }
    }
}

impl AnalysisScheduler {
    pub fn submit(&mut self, job: AnalysisJob) -> bool {
        job.window.is_valid() && self.submitted.insert(job.idempotency_key())
    }

    /// Returns analysis kinds due at `now_ms`; callers execute them in a
    /// background task, keeping the decode path independent.
    pub fn take_due(&mut self, now_ms: u64) -> Vec<AnalysisKind> {
        let mut due = Vec::new();
        for (&kind, interval) in &self.intervals {
            let interval_ms = interval.as_millis() as u64;
            let last = self.last_run_ms.get(&kind).copied().unwrap_or(0);
            if now_ms.saturating_sub(last) >= interval_ms {
                self.last_run_ms.insert(kind, now_ms);
                due.push(kind);
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> AggregateWindow {
        AggregateWindow {
            start_ms: 1,
            end_ms: 2,
            observations: 30,
            independent_model_observations: 2,
            disk_savings_percent: 20.0,
            baseline_decode_p95_ms: 100,
            candidate_decode_p95_ms: 105,
            confidence: 0.9,
        }
    }

    #[test]
    fn scheduler_accepts_only_aggregate_valid_windows_and_is_idempotent() {
        let mut scheduler = AnalysisScheduler::default();
        let job = AnalysisJob {
            kind: AnalysisKind::MoeHotset,
            window: window(),
        };
        assert!(scheduler.submit(job.clone()));
        assert!(!scheduler.submit(job));
        assert!(!scheduler.submit(AnalysisJob {
            kind: AnalysisKind::MoeHotset,
            window: AggregateWindow {
                observations: 0,
                ..window()
            }
        }));
    }

    #[test]
    fn due_jobs_are_rate_limited() {
        let mut scheduler = AnalysisScheduler::default();
        assert!(scheduler
            .take_due(60_000)
            .contains(&AnalysisKind::TransferBottleneck));
        assert!(scheduler.take_due(60_001).is_empty());
    }

    #[test]
    fn invalid_window_rejects_non_finite_or_reversed_metrics() {
        assert!(!AggregateWindow {
            end_ms: 1,
            ..window()
        }
        .is_valid());
        assert!(!AggregateWindow {
            confidence: f64::NAN,
            ..window()
        }
        .is_valid());
    }
}
