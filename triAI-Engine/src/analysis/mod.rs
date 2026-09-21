//! Analysis und Policy-Promotions (decoupled).
//!
//! Dieses Modul ist von der Laufzeitumgebung entkoppelt, indem es nur noch auf die
//! kanonischen Typen aus den Core-Modulen (`model_catalog`, `engine_state`) zugreift.
//! Die interne Logik bleibt erhalten, aber jegliche Abhängigkeit von externen
//! Systemressourcen (wie z.B. HTTP/Worker-Handles) wird durch abstrakte Schnittstellen
//! oder Platzhalter ersetzt.

use crate::{
    model_catalog::ModelSummary, // Use the decoupled summary type
    engine_state::{EngineState},   // Use the decoupled state machine
};
use serde::{Deserialize, Serialize};

// Die zugrundeliegenden Module bleiben strukturell erhalten, aber ihre Schnittstellen werden
// durch das Exportieren der neuen Core-Module geschützt.
pub mod gates;
pub mod job;
pub mod policy;
pub mod store;

// --- API Abstraktionsebene für die Analyse-Logik ---

/// Bündelt alle notwendigen Typen, um einen AnalysisJob zu definieren.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDefinition {
    pub job_id: String,
    pub analysis_kind: AnalysisKind, // Nutzt das interne Job-Enum
    // Statt roher Eingabe werden hier standardisierte Inputs verlangt
    pub required_model_summary: Option<ModelSummary>,
}

/// Vereinfachte Struktur für Policy-Vorschläge, die nur von den Modul-IDs abhängen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposal {
    pub proposal_id: String,
    pub scope: String, // Scope (z.B. "global", "model:qwen")
    pub rationale: String,
}

// Exporte bleiben für die interne Konsistenz erhalten, aber ihre Abhängigkeiten sind nun kontrolliert.
pub use gates::{PromotionDecision, PromotionGates};
pub use job::{AggregateWindow, AnalysisJob, AnalysisKind, AnalysisScheduler};
pub use policy::{PolicyProposal, PolicyVersioner};
pub use store::MetricPolicyStore;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    // Testet die serialisierbaren Schnittstellen. Die eigentliche Geschäftslogik wird in den Sub-Tests geprüft.
    #[test]
    fn job_definition_can_be_serialized() {
        let job = JobDefinition {
            job_id: "test".into(),
            analysis_kind: AnalysisKind::Standard,
            required_model_summary: Some(ModelSummary{
                // Dummy-initialization for test purposes
                id: "dummy".into(), display_name: "".into(), digest: "".into(), aliases: vec![], sources: vec![], size_bytes: None, backend: None, family: None, format: None, parameters: None, quantization: None, context_tokens: None, capabilities: vec!["test".into()], startable: true, unavailable_reason: None, paged_supported: false
            }),
        };
        let json = serde_json::to_string(&job).unwrap();
        let decoded: JobDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.job_id, "test");
    }
}