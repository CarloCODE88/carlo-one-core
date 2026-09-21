//! Explicit promotion and rollback gates for analysis proposals.

use super::{job::AggregateWindow, policy::PolicyProposal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionDecision {
    Promote,
    Hold(&'static str),
    Rollback(&'static str),
}

#[derive(Debug, Clone)]
pub struct PromotionGates {
    pub min_observations: u64,
    pub min_independent_models: u32,
    pub min_confidence: f64,
    pub min_disk_savings_percent: f64,
    pub max_decode_regression_percent: f64,
    pub rollback_decode_regression_percent: f64,
}

impl Default for PromotionGates {
    fn default() -> Self {
        Self {
            min_observations: 30,
            min_independent_models: 2,
            min_confidence: 0.8,
            min_disk_savings_percent: 15.0,
            max_decode_regression_percent: 10.0,
            rollback_decode_regression_percent: 20.0,
        }
    }
}

impl PromotionGates {
    pub fn evaluate(
        &self,
        window: AggregateWindow,
        proposal: &PolicyProposal,
    ) -> PromotionDecision {
        if proposal.decode_change_percent >= self.rollback_decode_regression_percent {
            return PromotionDecision::Rollback("decode regression exceeds rollback threshold");
        }
        if window.observations < self.min_observations {
            return PromotionDecision::Hold("insufficient observations");
        }
        if window.independent_model_observations < self.min_independent_models {
            return PromotionDecision::Hold("insufficient independent observations");
        }
        if proposal.confidence < self.min_confidence {
            return PromotionDecision::Hold("confidence below promotion threshold");
        }
        if proposal.disk_savings_percent < self.min_disk_savings_percent {
            return PromotionDecision::Hold("disk savings below promotion threshold");
        }
        if proposal.decode_change_percent > self.max_decode_regression_percent {
            return PromotionDecision::Hold("decode regression exceeds promotion threshold");
        }
        PromotionDecision::Promote
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn proposal(confidence: f64, disk: f64, change: f64) -> PolicyProposal {
        PolicyProposal {
            version: 1,
            source_window_start_ms: 1,
            source_window_end_ms: 2,
            cache_budget_mb: 512,
            prefetch_depth: 1,
            confidence,
            disk_savings_percent: disk,
            decode_change_percent: change,
        }
    }
    fn window() -> AggregateWindow {
        AggregateWindow {
            start_ms: 1,
            end_ms: 2,
            observations: 30,
            independent_model_observations: 2,
            disk_savings_percent: 20.0,
            baseline_decode_p95_ms: 100,
            candidate_decode_p95_ms: 100,
            confidence: 0.9,
        }
    }
    #[test]
    fn gates_require_confidence_savings_and_no_regression() {
        let gates = PromotionGates::default();
        assert_eq!(
            gates.evaluate(window(), &proposal(0.9, 15.0, 10.0)),
            PromotionDecision::Promote
        );
        assert_eq!(
            gates.evaluate(window(), &proposal(0.7, 20.0, 0.0)),
            PromotionDecision::Hold("confidence below promotion threshold")
        );
        assert_eq!(
            gates.evaluate(window(), &proposal(0.9, 14.9, 0.0)),
            PromotionDecision::Hold("disk savings below promotion threshold")
        );
        assert_eq!(
            gates.evaluate(window(), &proposal(0.9, 20.0, 20.0)),
            PromotionDecision::Rollback("decode regression exceeds rollback threshold")
        );
    }

    #[test]
    fn insufficient_or_non_independent_evidence_is_held() {
        let gates = PromotionGates::default();
        assert_eq!(
            gates.evaluate(
                AggregateWindow {
                    observations: 29,
                    ..window()
                },
                &proposal(0.9, 20.0, 0.0)
            ),
            PromotionDecision::Hold("insufficient observations")
        );
        assert_eq!(
            gates.evaluate(
                AggregateWindow {
                    independent_model_observations: 1,
                    ..window()
                },
                &proposal(0.9, 20.0, 0.0)
            ),
            PromotionDecision::Hold("insufficient independent observations")
        );
    }
}
