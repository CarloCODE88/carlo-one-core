//! Bounded, untrusted live-advice contract for the primary mini model.

use serde::Deserialize;
use std::time::Duration;

pub const LIVE_ADVICE_BUDGET: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advice {
    pub knowledge_area: KnowledgeArea,
    pub task_type: TaskType,
    pub required_tools: Vec<ReadOnlyTool>,
    pub context_quality: ContextQuality,
    pub active_area_estimate: ActiveAreaEstimate,
    pub prefetch_depth: u8,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeArea {
    SoftwareEngineering,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    CodeReview,
    CodeGeneration,
    Debugging,
    Documentation,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyTool {
    ReadFile,
    SearchFiles,
    ListFiles,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextQuality {
    RepositoryGrounded,
    FileLocal,
    Global,
    Empty,
}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAreaEstimate {
    DenseLayerWindow,
    MoeHotset,
    MetadataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdviceFailure {
    Timeout,
    InvalidJson,
    InvalidSchema,
    VramPressure,
    PrimaryUnavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdviceDecision {
    Accepted(Advice),
    Deterministic(AdviceFailure),
}

pub fn validate_live_advice(raw: &str, elapsed: Duration, vram_pressure: bool) -> AdviceDecision {
    if vram_pressure {
        return AdviceDecision::Deterministic(AdviceFailure::VramPressure);
    }
    if elapsed > LIVE_ADVICE_BUDGET {
        return AdviceDecision::Deterministic(AdviceFailure::Timeout);
    }
    let Ok(advice) = serde_json::from_str::<Advice>(raw) else {
        return AdviceDecision::Deterministic(AdviceFailure::InvalidJson);
    };
    if advice.required_tools.len() > 3
        || advice.prefetch_depth > 2
        || !advice.confidence.is_finite()
        || !(0.0..=1.0).contains(&advice.confidence)
    {
        return AdviceDecision::Deterministic(AdviceFailure::InvalidSchema);
    }
    AdviceDecision::Accepted(advice)
}

#[cfg(test)]
mod tests {
    use super::*;
    const VALID: &str = r#"{"knowledge_area":"software_engineering","task_type":"code_review","required_tools":["read_file"],"context_quality":"repository_grounded","active_area_estimate":"dense_layer_window","prefetch_depth":1,"confidence":0.8}"#;
    #[test]
    fn advice_is_bounded_and_fails_closed() {
        assert!(matches!(
            validate_live_advice(VALID, Duration::from_millis(20), false),
            AdviceDecision::Accepted(_)
        ));
        assert!(matches!(
            validate_live_advice(VALID, Duration::from_millis(21), false),
            AdviceDecision::Deterministic(AdviceFailure::Timeout)
        ));
        assert!(matches!(
            validate_live_advice("{}", Duration::ZERO, false),
            AdviceDecision::Deterministic(AdviceFailure::InvalidJson)
        ));
        assert!(matches!(
            validate_live_advice(VALID, Duration::ZERO, true),
            AdviceDecision::Deterministic(AdviceFailure::VramPressure)
        ));
    }
}
