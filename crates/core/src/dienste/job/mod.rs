//! CarloONE 2.0 Job-Orchestra
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarloJobState {
    Queued,
    Running,
    Preview,
    AwaitingAccept,
    Completed,
    Failed(String),
}

impl From<tri_ai_engine::api::JobStatus> for CarloJobState {
    fn from(status: tri_ai_engine::api::JobStatus) -> Self {
        match status {
            tri_ai_engine::api::JobStatus::Idle => CarloJobState::Queued,
            tri_ai_engine::api::JobStatus::Loading => CarloJobState::Running,
            tri_ai_engine::api::JobStatus::Busy => CarloJobState::Running,
            tri_ai_engine::api::JobStatus::Ready => CarloJobState::Preview,
            tri_ai_engine::api::JobStatus::Error(msg) => CarloJobState::Failed(msg),
        }
    }
}
