//! HIXX Telemetrie-Adapter
//! Optional – Wird in Phase 3 implementiert
#[derive(Debug, Clone, serde::Serialize)]
pub struct TelemetrySnapshot {
    pub gpu_usage_percent: f32,
    pub gpu_memory_mb: u64,
    pub cpu_usage_percent: f32,
    pub cpu_temp_celsius: f32,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
