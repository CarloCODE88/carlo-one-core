//! Job-Fehlertypen
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("Job nicht gefunden: {0}")]
    NotFound,
    #[error("Job konnte nicht gestartet werden: {0}")]
    SubmitFailed(String),
    #[error("Job konnte nicht abgebrochen werden: {0}")]
    CancelFailed(String),
    #[error("Ungültiger Job-Zustand: {0}")]
    InvalidState(String),
    #[error("Engine nicht erreichbar")]
    EngineUnavailable,
}
