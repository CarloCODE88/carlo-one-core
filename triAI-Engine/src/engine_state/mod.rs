//! Module Definition for the EngineState module
// Export all necessary components used by external parts of the engine.
pub use super::engine_state::Engine;
pub use super::engine_state::WorkerConfig;
pub use super::engine_state::{EngineState, WorkerFailure};

/// Aggregiert alle Status- und Konfigurationsstrukturen für den Engine-Teil des Projekts.
// Wenn externe Module Zugriff auf die State Machine benötigen, werden sie über diese API angebunden.