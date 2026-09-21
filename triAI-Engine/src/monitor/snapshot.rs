//! System-Status-Erfassung (RAM, VRAM, CPU, Queue-Depth).
//! Non-blocking: Snapshot-Erfassung darf Decode nicht blockieren.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tokio::time::interval;

/// Aktueller System-Zustand
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub timestamp: DateTime<Utc>,
    pub vram_used_mb: u32,
    pub vram_total_mb: u32,
    pub ram_used_mb: u32,
    pub ram_total_mb: u32,
    pub pinned_ram_used_mb: u32,
    pub pinned_ram_limit_mb: u32,
    pub kv_cache_mb: u32,
    pub queue_depth: u32,
    pub cpu_usage_percent: f32,
    pub active_model_id: Option<String>,
    pub decode_in_progress: bool,
}

impl ResourceSnapshot {
    pub fn vram_ratio(&self) -> f64 {
        if self.vram_total_mb == 0 {
            return 0.0;
        }
        self.vram_used_mb as f64 / self.vram_total_mb as f64
    }

    pub fn ram_ratio(&self) -> f64 {
        if self.ram_total_mb == 0 {
            return 0.0;
        }
        self.ram_used_mb as f64 / self.ram_total_mb as f64
    }
}

/// System-Monitor: Erfasst periodisch Snapshots
pub struct SystemMonitor {
    system: Arc<Mutex<System>>,
    latest_snapshot: Arc<Mutex<Option<ResourceSnapshot>>>,
    vram_total_mb: u32,
    ram_total_mb: u32,
    pinned_ram_limit_mb: u32,
}

impl SystemMonitor {
    pub fn new(vram_total_mb: u32, pinned_ram_limit_mb: u32) -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::new()
                .with_memory(MemoryRefreshKind::everything())
                .with_cpu(CpuRefreshKind::everything()),
        );
        sys.refresh_all();

        let ram_total_mb = (sys.total_memory() / 1024 / 1024) as u32;

        Self {
            system: Arc::new(Mutex::new(sys)),
            latest_snapshot: Arc::new(Mutex::new(None)),
            vram_total_mb,
            ram_total_mb,
            pinned_ram_limit_mb,
        }
    }

    /// Startet periodische Snapshot-Erfassung im Hintergrund
    pub async fn run_periodic(self: Arc<Self>, interval_ms: u64) {
        let mut ticker = interval(Duration::from_millis(interval_ms));

        loop {
            ticker.tick().await;
            let snapshot = self.capture_snapshot().await;

            let mut latest = self.latest_snapshot.lock().unwrap();
            *latest = Some(snapshot);
        }
    }

    /// Erfasst einen Snapshot (non-blocking)
    pub async fn capture_snapshot(&self) -> ResourceSnapshot {
        let (ram_used, cpu_usage) = {
            let mut sys = self.system.lock().unwrap();
            sys.refresh_memory();
            sys.refresh_cpu_usage();

            let ram_used = (sys.used_memory() / 1024 / 1024) as u32;
            let cpu_usage = sys.global_cpu_info().cpu_usage();
            (ram_used, cpu_usage)
        };

        // VRAM: In Produktion via NVML/cuda::MemGetInfo
        // Für MVP: Schätzung basierend auf aktiven Modellen
        let (vram_used, active_model, decode_in_progress) = self.estimate_vram_usage().await;

        ResourceSnapshot {
            timestamp: Utc::now(),
            vram_used_mb: vram_used,
            vram_total_mb: self.vram_total_mb,
            ram_used_mb: ram_used,
            ram_total_mb: self.ram_total_mb,
            pinned_ram_used_mb: 0, // TODO: Track via staging.rs
            pinned_ram_limit_mb: self.pinned_ram_limit_mb,
            kv_cache_mb: 0, // TODO: Track via engine.rs
            queue_depth: 0, // TODO: Track via worker-queue
            cpu_usage_percent: cpu_usage,
            active_model_id: active_model,
            decode_in_progress,
        }
    }

    /// Holt den letzten Snapshot (non-blocking)
    pub fn get_latest(&self) -> Option<ResourceSnapshot> {
        self.latest_snapshot.lock().unwrap().clone()
    }

    /// VRAM-Schätzung (MVP: statisch, Produktion: NVML)
    async fn estimate_vram_usage(&self) -> (u32, Option<String>, bool) {
        // TODO: Integration mit model_registry.rs
        // Für jetzt: Placeholder
        (0, None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vram_ratio() {
        let snapshot = ResourceSnapshot {
            timestamp: Utc::now(),
            vram_used_mb: 8000,
            vram_total_mb: 11264, // 11 GB
            ram_used_mb: 16000,
            ram_total_mb: 32768,
            pinned_ram_used_mb: 0,
            pinned_ram_limit_mb: 4096,
            kv_cache_mb: 0,
            queue_depth: 0,
            cpu_usage_percent: 50.0,
            active_model_id: None,
            decode_in_progress: false,
        };

        let ratio = snapshot.vram_ratio();
        assert!(ratio > 0.7 && ratio < 0.8);
    }

    #[tokio::test]
    async fn test_system_monitor_capture() {
        let monitor = SystemMonitor::new(11264, 4096);
        let snapshot = monitor.capture_snapshot().await;

        assert!(snapshot.ram_total_mb > 0);
        assert_eq!(snapshot.vram_total_mb, 11264);
    }

    #[test]
    fn test_ram_ratio_zero_total() {
        let snapshot = ResourceSnapshot {
            timestamp: Utc::now(),
            vram_used_mb: 0,
            vram_total_mb: 0,
            ram_used_mb: 0,
            ram_total_mb: 0,
            pinned_ram_used_mb: 0,
            pinned_ram_limit_mb: 0,
            kv_cache_mb: 0,
            queue_depth: 0,
            cpu_usage_percent: 0.0,
            active_model_id: None,
            decode_in_progress: false,
        };

        assert_eq!(snapshot.vram_ratio(), 0.0);
        assert_eq!(snapshot.ram_ratio(), 0.0);
    }
}
