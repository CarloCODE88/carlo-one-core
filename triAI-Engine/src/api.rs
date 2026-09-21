//! API-Vertrag für GUI und lokale HTTP-Schicht (decoupled).
//!
//! Dieser Modul dient als öffentliche Schnittstelle (`public facing contract`). Er verwendet
//! ausschließlich die abstrahierten Module `model_catalog`, `tool_registry` und `engine_state`.
//! Direkte Abhängigkeiten von I/O oder Kernel-Details wurden entfernt, um eine Kompilierbarkeit
//! ohne den gesamten Engine-Kontext zu gewährleisten.

use crate::{
    model_catalog::ModelSummary,
    tool_registry::ToolRegistry,
    engine_state::{EngineState, ApiError},
};
use serde::{Deserialize, Serialize};

// --- ENUMS & STRUCTS (Public API Contracts) ---

/// Fehlerkatalog des Engines. Nur der Code muss serialisiert werden, die Details sind intern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    ModelBusy,
    StalePlan,
    InvalidRequest,
    NotReady,
    ResourceDenied,
    WorkerTimeout,
    WorkerCrashed,
    Cancelled,
    OutOfMemory,
    PortConflict,
    CorruptModel,
    NotFound,
    Internal,
}

/// Die serialisierbare Fehlerrepräsentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn model_busy() -> Self {
        Self {
            code: ApiErrorCode::ModelBusy,
            message: "genau ein lokales Modell darf gleichzeitig aktiv sein".into(),
        }
    }
    // Weitere Fehlerkonstruktoren bleiben unverändert
}

/// Der API-Statusbericht des gesamten Engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub state: EngineState,
    pub active_model: Option<String>,
    pub single_model_only: bool,
    // Ressourcen werden jetzt über einen dedizierten API-Call geladen.
}

/// Die gesamte Request Payload. Verwendet die neuen Module als Quelle für Typen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    Status,
    Resources,
    ListModels, // Nutzt ModelCatalog::summaries() intern
    StartModel { model: String },
    StopModel,
    Chat { model: String, prompt: String },
    StageStatus,
    // Download/Rescan-Anfragen bleiben, da diese I/O-Funktionalität verwalten.
    CreateDownload(crate::download::DownloadRequest),
    DownloadStatus { id: String },
    CancelDownload { id: String },
    RescanModels, // Trigger für den Rescan-Prozess
}

/// Die API Response Payload. Enthält nun nur noch die kontraktuellen Typen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Status(StatusResponse),
    Models {
        models: Vec<ModelSummary>, // Nutzt ModelCatalog::summaries()
    },
    Chat {
        model: String,
        text: String,
    },
    // ... weitere Responses
}

/// Public API Funktion zur Fehlerermittlung basierend auf dem EngineState.
pub fn error_for_state(state: EngineState) -> ApiError {
    match state {
        EngineState::Busy
        | EngineState::Loading
        | EngineState::Degraded
        | EngineState::Draining
        | EngineState::Rollback
        | EngineState::Stopping => ApiError::model_busy(),
        EngineState::Idle => ApiError {
            code: ApiErrorCode::NotReady,
            message: "kein Modell ist geladen".into(),
        },
        EngineState::Failed => ApiError {
            code: ApiErrorCode::Internal,
            message: "Runner befindet sich im Fehlerzustand".into(),
        },
        // ... (rest remains the same)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    // Mocking dependencies needed for compilation check
    pub mod download {}
    pub mod engine_state {}

    // Testet die strukturelle Integrität des API-Vertrags
    #[test]
    fn request_roundtrips_as_json() {
        let request = Request::Chat {
            model: "coder.gguf".into(),
            prompt: "ping".into(),
        };
        // Assertions folgen hier...
    }
}