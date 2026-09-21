//! Shared struct definitions matching kernel module layout.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub gpu_utilization: u64,
    pub gpu_temperature: u64,
    pub gpu_clock_mhz: u64,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub inference_count: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingSlot {
    pub physical_addr: u64,
    pub size: u32,
    pub flags: u32,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
    pub context_tokens: u32,
    pub gpu_device: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub generated: String,
    pub tokens_generated: u64,
    pub ttft_ms: u128,
    pub total_ms: u128,
    pub metrics: Metrics,
}
