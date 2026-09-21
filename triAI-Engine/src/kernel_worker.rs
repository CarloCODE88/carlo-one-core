//! Kernel-Worker: Echtzeit VRAM/RAM/Memory-Management fuer die triAI-Engine.
//!
//! Verantwortlich fuer:
//! - VRAM-Benutzungsueberwachung via nvidia-smi
//! - Intelligentes Eviction (Hot/Cold-Chunks)
//! - Expert-getriebenes Prefetching
//! - Memory-Pressure-Events an den Supervisor
//! - Nahtloses Offloading waehrend aktiver Inferenz

use crate::error::{Result, TriAIError};
use crate::supervisor::expert_tracker::ExpertEvent;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ────────────────────────────────────────────────────────────
// Datentypen
// ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub chunk_id: String,
    pub tensor_name: String,
}

#[derive(Debug, Clone)]
pub struct VramPage {
    pub key: PageKey,
    pub size_bytes: u64,
    pub loaded_at: Instant,
    pub last_accessed: Instant,
    pub access_count: u64,
    pub is_hot: bool,
    pub pin_count: u32,
}

#[derive(Debug, Clone)]
pub struct RamPage {
    pub key: PageKey,
    pub size_bytes: u64,
    pub evicted_at: Instant,
    pub disk_offset: u64,
    pub access_since_eviction: u64,
}

#[derive(Debug, Clone)]
pub struct DiskPage {
    pub key: PageKey,
    pub size_bytes: u64,
    pub written_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryTier {
    Vram,
    Ram,
    Disk,
}

#[derive(Debug, Clone)]
pub struct MemoryEvent {
    pub tier_from: MemoryTier,
    pub tier_to: MemoryTier,
    pub key: PageKey,
    pub size_bytes: u64,
    pub timestamp: Instant,
    pub reason: MemoryEventReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEventReason {
    Load,
    Evict,
    Prefetch,
    ExpertRouting,
    PressureEviction,
    Pin,
    Unpin,
}

// ────────────────────────────────────────────────────────────
// GPU-Speicher-Informationen (nvidia-smi)
// ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GpuMemoryInfo {
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
    pub gpu_util_percent: u8,
    pub memory_util_percent: u8,
    pub performance_state: String,
}

impl GpuMemoryInfo {
    pub fn query() -> Result<Self> {
        let output = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=memory.total,memory.used,memory.free,utilization.gpu", "--format=csv,noheader,nounits"])
            .output()
            .map_err(|e| TriAIError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut total_mb = 0u64;
        let mut used_mb = 0u64;
        let mut gpu_util = 0u8;

        for line in stdout.lines() {
            let parts: Vec<&str> = line.trim().split(',').collect();
            if parts.len() >= 4 {
                if let Ok(t) = parts[0].trim().parse::<u64>() { total_mb = t; }
                if let Ok(u) = parts[1].trim().parse::<u64>() { used_mb = u; }
                if let Ok(g) = parts[3].trim().parse::<u8>() { gpu_util = g; }
            }
        }

        let perf_state = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=pcie.link.gen.max", "--format=csv,noheader,nounits"])
            .output()
            .map_err(|e| TriAIError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let perf_state = String::from_utf8_lossy(&perf_state.stdout).trim().to_string();

        Ok(Self {
            total_mb,
            used_mb,
            free_mb: total_mb.saturating_sub(used_mb),
            gpu_util_percent: gpu_util,
            memory_util_percent: if total_mb > 0 { (used_mb * 100 / total_mb) as u8 } else { 0 },
            performance_state: perf_state,
        })
    }
}

// ────────────────────────────────────────────────────────────
// KernelWorker - Hauptstruktur
// ────────────────────────────────────────────────────────────

pub struct KernelWorker {
    archive: PathBuf,
    vram_pages: Arc<Mutex<HashMap<String, VramPage>>>,
    ram_pages: Arc<Mutex<HashMap<String, RamPage>>>,
    disk_pages: Arc<Mutex<HashMap<String, DiskPage>>>,
    events: Arc<Mutex<Vec<MemoryEvent>>>,
    hot_keys: Arc<Mutex<HashSet<String>>>,
    cold_keys: Arc<Mutex<HashSet<String>>>,
    pinned_keys: Arc<Mutex<HashSet<String>>>,
    vram_budget_bytes: u64,
    monitoring: Arc<Mutex<bool>>,
}

impl KernelWorker {
    pub fn new(archive: impl AsRef<Path>, vram_budget_mb: u64) -> Self {
        Self {
            archive: archive.as_ref().to_path_buf(),
            vram_pages: Arc::new(Mutex::new(HashMap::new())),
            ram_pages: Arc::new(Mutex::new(HashMap::new())),
            disk_pages: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            hot_keys: Arc::new(Mutex::new(HashSet::new())),
            cold_keys: Arc::new(Mutex::new(HashSet::new())),
            pinned_keys: Arc::new(Mutex::new(HashSet::new())),
            vram_budget_bytes: vram_budget_mb * 1024 * 1024,
            monitoring: Arc::new(Mutex::new(false)),
        }
    }

    /// Startet kontinuierliches VRAM-Monitoring (laeuft im Hintergrund-Thread).
    /// Pruft alle `interval_ms` Millisekunden den GPU-Speicher.
    /// Wenn Druck > 85%, evakuiert automatisch kalte Seiten.
    pub fn start_monitoring(&self, interval_ms: u64) {
        let monitoring = self.monitoring.clone();
        let vram_pages = self.vram_pages.clone();
        let ram_pages = self.ram_pages.clone();
        let events = self.events.clone();
        let pinned_keys = self.pinned_keys.clone();
        let budget = self.vram_budget_bytes;

        *self.monitoring.lock().unwrap() = true;
        thread::spawn(move || {
            while *monitoring.lock().unwrap() {
                if let Ok(gpu_info) = GpuMemoryInfo::query() {
                    let pressure = gpu_info.memory_util_percent >= 85;
                    if pressure {
                        let mut ev = events.lock().unwrap();
                        ev.push(MemoryEvent {
                            tier_from: MemoryTier::Vram,
                            tier_to: MemoryTier::Ram,
                            key: PageKey {
                                chunk_id: "pressure_evict".to_string(),
                                tensor_name: "auto".to_string(),
                            },
                            size_bytes: 0,
                            timestamp: Instant::now(),
                            reason: MemoryEventReason::PressureEviction,
                        });
                        drop(ev);
                        Self::evict_cold_entries(
                            &vram_pages,
                            &ram_pages,
                            &pinned_keys,
                            budget,
                        );
                    }
                }
                thread::sleep(Duration::from_millis(interval_ms));
            }
        });
    }

    pub fn stop_monitoring(&self) {
        *self.monitoring.lock().unwrap() = false;
    }

    pub fn is_monitored(&self) -> bool {
        *self.monitoring.lock().unwrap()
    }

    /// Laedt einen Chunk in VRAM. Evakuiert automatisch kalte Seiten bei Budget-Ueberschreitung.
    pub fn load_to_vram(&self, chunk_id: &str, tensor_name: &str, size: u64) -> Result<()> {
        let key = chunk_id.to_string();
        let page_key = PageKey {
            chunk_id: chunk_id.to_string(),
            tensor_name: tensor_name.to_string(),
        };

        let vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;

        if vram.contains_key(&key) {
            drop(vram);
            self.record_access(&key);
            return Ok(());
        }

        let vram_bytes: u64 = vram.values().map(|p| p.size_bytes).sum();
        drop(vram);

        if vram_bytes + size > self.vram_budget_bytes {
            KernelWorker::evict_cold_entries(
                &self.vram_pages,
                &self.ram_pages,
                &self.pinned_keys,
                self.vram_budget_bytes,
            );
        }

        let now = Instant::now();
        let mut vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;
        vram.insert(
            key.clone(),
            VramPage {
                key: page_key.clone(),
                size_bytes: size,
                loaded_at: now,
                last_accessed: now,
                access_count: 1,
                is_hot: false,
                pin_count: 0,
            },
        );
        drop(vram);

        let mut events = self.events.lock().map_err(|_| TriAIError::Other("events lock poisoned".into()))?;
        events.push(MemoryEvent {
            tier_from: MemoryTier::Disk,
            tier_to: MemoryTier::Vram,
            key: page_key,
            size_bytes: size,
            timestamp: now,
            reason: MemoryEventReason::Load,
        });
        drop(events);

        Ok(())
    }

    /// Evaquiert einen Chunk von VRAM nach RAM.
    pub fn evict_to_ram(&self, chunk_id: &str) -> Result<()> {
        let pinned = self.pinned_keys.lock().map_err(|_| TriAIError::Other("pinned lock poisoned".into()))?;
        if pinned.contains(chunk_id) {
            drop(pinned);
            return Ok(());
        }
        drop(pinned);
        let mut vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;
        let mut ram = self.ram_pages.lock().map_err(|_| TriAIError::Other("ram lock poisoned".into()))?;
        let now = Instant::now();

        if let Some(page) = vram.remove(chunk_id) {
            let key_clone = page.key.clone();
            ram.insert(
                chunk_id.to_string(),
                RamPage {
                    key: page.key,
                    size_bytes: page.size_bytes,
                    evicted_at: now,
                    disk_offset: 0,
                    access_since_eviction: 0,
                },
            );
            drop(vram);
            drop(ram);
            let mut events = self.events.lock().map_err(|_| TriAIError::Other("events lock poisoned".into()))?;
            events.push(MemoryEvent {
                tier_from: MemoryTier::Vram,
                tier_to: MemoryTier::Ram,
                key: key_clone,
                size_bytes: page.size_bytes,
                timestamp: now,
                reason: MemoryEventReason::Evict,
            });
            drop(events);
        } else {
            drop(vram);
            drop(ram);
        }
        Ok(())
    }

    /// Prueft ob ein Chunk heiss genug fuer aktiven Prefetch ist.
    pub fn classify_experts(&self, events: &[ExpertEvent]) {
        let mut access_counts: HashMap<String, u64> = HashMap::new();
        for event in events {
            for expert_id in &event.expert_ids {
                let key = format!("{}_{}", event.layer_id, expert_id);
                *access_counts.entry(key).or_insert(0) += 1;
            }
        }

        let mut hot = HashSet::new();
        let mut cold = HashSet::new();
        let threshold = 3;

        for (key, count) in &access_counts {
            if *count >= threshold {
                hot.insert(key.clone());
            } else {
                cold.insert(key.clone());
            }
        }

        *self.hot_keys.lock().unwrap() = hot;
        *self.cold_keys.lock().unwrap() = cold;
    }

    /// Prefetcht Expert-Tensoren in VRAM basierend auf Routing-Events.
    /// Dies ist der Kern-Mechanismus fuer fluediges Offloading waehrend Inferenz.
    pub fn prefetch_experts(&self, events: &[ExpertEvent]) -> Result<()> {
        for event in events {
            for expert_id in &event.expert_ids {
                let tensor_name = format!("blk.{}.ffn_gate.{}", event.layer_id, expert_id);
                let chunk_id = format!("expert_{}_{}", event.layer_id, expert_id);

                let already_loaded = {
                    let vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;
                    vram.contains_key(&chunk_id)
                };

                if !already_loaded {
                    let _ = self.load_to_vram(&chunk_id, &tensor_name, 64 * 1024 * 1024);
                } else {
                    self.record_access(&chunk_id);
                }
            }
        }
        Ok(())
    }

    /// Synchronisiert Expert-Events mit dem Memory-Manager.
    /// Klassifiziert Experten und prefetcht heisse Experten.
    pub fn sync_expert_events(&self, events: &[ExpertEvent]) -> Result<()> {
        self.classify_experts(events);
        self.prefetch_experts(events)
    }

    /// Erhoeht den Zugriffs-Count fuer einen Chunk.
    pub fn record_access(&self, chunk_id: &str) {
        let mut vram = self.vram_pages.lock().unwrap();
        if let Some(page) = vram.get_mut(chunk_id) {
            page.access_count += 1;
            page.last_accessed = Instant::now();
            page.is_hot = page.access_count > 3;
        }
    }

    /// Pinnt einen Chunk in VRAM (kein Eviction moeglich).
    pub fn pin_chunk(&self, chunk_id: &str) -> Result<()> {
        let mut vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;
        let mut pinned = self.pinned_keys.lock().map_err(|_| TriAIError::Other("pinned lock poisoned".into()))?;
        if let Some(page) = vram.get_mut(chunk_id) {
            page.pin_count += 1;
            pinned.insert(chunk_id.to_string());
        }
        Ok(())
    }

    /// Entpinnt einen Chunk.
    pub fn unpin_chunk(&self, chunk_id: &str) -> Result<()> {
        let mut vram = self.vram_pages.lock().map_err(|_| TriAIError::Other("vram lock poisoned".into()))?;
        let mut pinned = self.pinned_keys.lock().map_err(|_| TriAIError::Other("pinned lock poisoned".into()))?;
        if let Some(page) = vram.get_mut(chunk_id) {
            if page.pin_count > 0 {
                page.pin_count -= 1;
            }
            if page.pin_count == 0 {
                pinned.remove(chunk_id);
            }
        }
        Ok(())
    }

    /// Gibt den aktuellen VRAM/RAM-Druck zurueck.
    pub fn memory_pressure(&self) -> MemoryPressure {
        let vram = self.vram_pages.lock().unwrap();
        let ram = self.ram_pages.lock().unwrap();
        let vram_bytes: u64 = vram.values().map(|p| p.size_bytes).sum();
        let ram_bytes: u64 = ram.values().map(|p| p.size_bytes).sum();
        let vram_count = vram.len();
        let ram_count = ram.len();

        let gpu_info = GpuMemoryInfo::query().unwrap_or_else(|_| GpuMemoryInfo {
            total_mb: 11264,
            used_mb: 0,
            free_mb: 11264,
            gpu_util_percent: 0,
            memory_util_percent: 0,
            performance_state: "unknown".to_string(),
        });

        let vram_pressure = vram_bytes >= self.vram_budget_bytes || gpu_info.memory_util_percent >= 85;
        let ram_pressure = ram_count > 50;

        MemoryPressure {
            vram_bytes,
            vram_count,
            ram_bytes,
            ram_count,
            vram_budget_bytes: self.vram_budget_bytes,
            vram_util_percent: if self.vram_budget_bytes > 0 {
                (vram_bytes * 100 / self.vram_budget_bytes) as u8
            } else {
                0
            },
            gpu_util_percent: gpu_info.memory_util_percent,
            vram_pressure,
            ram_pressure,
            overall_pressure: vram_pressure || ram_pressure,
        }
    }

    /// Gibt einen vollstaendigen Report ueber den aktuellen Speicherzustand.
    pub fn get_report(&self) -> KernelReport {
        let vram = self.vram_pages.lock().unwrap();
        let ram = self.ram_pages.lock().unwrap();
        let disk = self.disk_pages.lock().unwrap();
        let events = self.events.lock().unwrap();
        let hot = self.hot_keys.lock().unwrap();
        let cold = self.cold_keys.lock().unwrap();
        let pinned = self.pinned_keys.lock().unwrap();

        let vram_bytes: u64 = vram.values().map(|p| p.size_bytes).sum();
        let ram_bytes: u64 = ram.values().map(|p| p.size_bytes).sum();
        let disk_bytes: u64 = disk.values().map(|p| p.size_bytes).sum();

        KernelReport {
            vram_pages: vram.len(),
            vram_bytes,
            ram_pages: ram.len(),
            ram_bytes,
            disk_pages: disk.len(),
            disk_bytes,
            total_events: events.len(),
            hot_count: hot.len(),
            cold_count: cold.len(),
            pinned_count: pinned.len(),
            vram_budget_bytes: self.vram_budget_bytes,
        }
    }

    // Interne Helper: Evaquiert kalte (nicht-heisse, nicht-pinnte) Eintraege aus VRAM.
    pub fn evict_cold_entries(
        vram: &Arc<Mutex<HashMap<String, VramPage>>>,
        ram: &Arc<Mutex<HashMap<String, RamPage>>>,
        pinned: &Arc<Mutex<HashSet<String>>>,
        _budget: u64,
    ) {
        let pinned_keys = pinned.lock().unwrap();
        let mut vram_guard = vram.lock().unwrap();
        let mut ram_guard = ram.lock().unwrap();
        let now = Instant::now();

        let cold_entries: Vec<String> = vram_guard
            .iter()
            .filter(|(k, _)| !pinned_keys.contains(*k))
            .filter(|(_, p)| !p.is_hot)
            .map(|(k, _)| k.clone())
            .collect();

        for key in cold_entries {
            if let Some(page) = vram_guard.remove(&key) {
                ram_guard.insert(
                    key.clone(),
                    RamPage {
                        key: page.key.clone(),
                        size_bytes: page.size_bytes,
                        evicted_at: now,
                        disk_offset: 0,
                        access_since_eviction: 0,
                    },
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────
// Berichtsstrukturen
// ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MemoryPressure {
    pub vram_bytes: u64,
    pub vram_count: usize,
    pub ram_bytes: u64,
    pub ram_count: usize,
    pub vram_budget_bytes: u64,
    pub vram_util_percent: u8,
    pub gpu_util_percent: u8,
    pub vram_pressure: bool,
    pub ram_pressure: bool,
    pub overall_pressure: bool,
}

#[derive(Debug, Clone)]
pub struct KernelReport {
    pub vram_pages: usize,
    pub vram_bytes: u64,
    pub ram_pages: usize,
    pub ram_bytes: u64,
    pub disk_pages: usize,
    pub disk_bytes: u64,
    pub total_events: usize,
    pub hot_count: usize,
    pub cold_count: usize,
    pub pinned_count: usize,
    pub vram_budget_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn kernel_worker_loads_and_evicts() {
        let worker = KernelWorker::new("/tmp/test", 100);
        worker.load_to_vram("chunk_001", "tensor_a", 1024).unwrap();
        worker.load_to_vram("chunk_002", "tensor_b", 1024).unwrap();
        let report = worker.get_report();
        assert_eq!(report.vram_pages, 2);
        assert_eq!(report.vram_bytes, 2048);
        worker.evict_to_ram("chunk_001").unwrap();
        let report = worker.get_report();
        assert_eq!(report.vram_pages, 1);
        assert_eq!(report.ram_pages, 1);
    }

    #[test]
    fn kernel_worker_prefetches_experts() {
        let worker = KernelWorker::new("/tmp/test", 100);
        let events = vec![ExpertEvent {
            layer_id: 0,
            expert_ids: vec![1, 2],
            timestamp_ms: 1000,
        }];
        worker.prefetch_experts(&events).unwrap();
        let report = worker.get_report();
        assert!(report.vram_pages >= 1);
    }

    #[test]
    fn memory_pressure_detection() {
        let worker = KernelWorker::new("/tmp/test", 1);
        worker.load_to_vram("chunk_001", "tensor_a", 1024 * 1024).unwrap();
        let pressure = worker.memory_pressure();
        assert!(pressure.overall_pressure);
    }

    #[test]
    fn pinning_preserves_chunks() {
        let worker = KernelWorker::new("/tmp/test", 100);
        worker.load_to_vram("chunk_001", "tensor_a", 1024).unwrap();
        worker.pin_chunk("chunk_001").unwrap();
        worker.evict_to_ram("chunk_001").unwrap();
        let report = worker.get_report();
        assert_eq!(report.vram_pages, 1);
    }

    #[test]
    fn monitoring_lifecycle() {
        let worker = KernelWorker::new("/tmp/test", 100);
        assert!(!worker.is_monitored());
        worker.start_monitoring(100);
        assert!(worker.is_monitored());
        thread::sleep(Duration::from_millis(250));
        worker.stop_monitoring();
        assert!(!worker.is_monitored());
    }
}
