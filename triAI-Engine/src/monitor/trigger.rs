//! Trigger-Engine: Wertet Schwellwerte aus und gibt Aktionen zurück.
//!
//! 8 Trigger aus `tri-routing-knowledge.json`:
//! - VRAM kritisch → Prefetch stoppen, kalte Region evicten
//! - VRAM fällt schnell → Prefetch reduzieren, Residency neu planen
//! - KV wächst stark → KV budgetieren, optionalen Cache verdrängen
//! - MoE-Hit-Rate niedrig → vorhergesagte Experten laden
//! - Prefetch falsch → spekulatives Prefetch deaktivieren
//! - Queue hoch → Transfers bündeln
//! - SSD im Decode → synchronen SSD-Read abbrechen
//! - Kontext fast voll → Kontext komprimieren/zusammenfassen

use crate::monitor::hysteresis::{CooldownConfig, HysteresisState};
use crate::monitor::snapshot::ResourceSnapshot;
use serde::{Deserialize, Serialize};

/// Trigger-Aktionen, die vom Engine-Core ausgeführt werden
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerAction {
    StopPrefetch,
    ReducePrefetch,
    EvictColdRegion,
    ReplanResidency,
    BudgetKV,
    EvictOptionalCache,
    LoadPredictedExperts,
    DisableSpeculativePrefetch,
    BatchTransfers,
    AbortSSDRead,
    CompressContext,
}

/// Konfigurierbare Schwellwerte
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    pub vram_critical_threshold: f64,        // 0.95
    pub vram_falling_threshold: f64,         // 0.85
    pub kv_cache_threshold_mb: u32,          // 4096
    pub moe_hit_rate_threshold: f64,         // 0.6
    pub queue_depth_threshold: u32,          // 10
    pub context_full_threshold_percent: f64, // 0.9
    pub cooldown_config: CooldownConfig,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            vram_critical_threshold: 0.95,
            vram_falling_threshold: 0.85,
            kv_cache_threshold_mb: 4096,
            moe_hit_rate_threshold: 0.6,
            queue_depth_threshold: 10,
            context_full_threshold_percent: 0.9,
            cooldown_config: CooldownConfig::default(),
        }
    }
}

/// Trigger-Engine mit Hysterese
pub struct TriggerEngine {
    config: TriggerConfig,
    hysteresis: HysteresisState,
    moe_hit_rate: f64,
    prefetch_accuracy: f64,
    context_usage_percent: f64,
}

impl TriggerEngine {
    pub fn new(config: TriggerConfig) -> Self {
        let cooldown = config.cooldown_config.clone();
        Self {
            config,
            hysteresis: HysteresisState::new(cooldown),
            moe_hit_rate: 1.0,
            prefetch_accuracy: 1.0,
            context_usage_percent: 0.0,
        }
    }

    /// Aktualisiert MoE-Hit-Rate (von engine.rs aufgerufen)
    pub fn update_moe_hit_rate(&mut self, hit_rate: f64) {
        self.moe_hit_rate = hit_rate;
    }

    /// Aktualisiert Prefetch-Genauigkeit (von chunk_loader.rs aufgerufen)
    pub fn update_prefetch_accuracy(&mut self, accuracy: f64) {
        self.prefetch_accuracy = accuracy;
    }

    /// Aktualisiert Kontext-Auslastung (von engine.rs aufgerufen)
    pub fn update_context_usage(&mut self, usage_percent: f64) {
        self.context_usage_percent = usage_percent;
    }

    /// Wertet Snapshot aus und gibt Trigger-Aktionen zurück
    pub fn evaluate(&mut self, snapshot: &ResourceSnapshot) -> Vec<TriggerAction> {
        let mut actions = Vec::new();
        let now = std::time::Instant::now();

        // Trigger 1: VRAM kritisch
        if snapshot.vram_ratio() > self.config.vram_critical_threshold {
            if self.hysteresis.can_trigger("vram_critical", now) {
                actions.push(TriggerAction::StopPrefetch);
                actions.push(TriggerAction::EvictColdRegion);
                self.hysteresis.record_trigger("vram_critical", now);
            }
        }
        // Trigger 2: VRAM fällt schnell (nur wenn nicht schon kritisch)
        else if snapshot.vram_ratio() > self.config.vram_falling_threshold {
            if self.hysteresis.can_trigger("vram_falling", now) {
                actions.push(TriggerAction::ReducePrefetch);
                actions.push(TriggerAction::ReplanResidency);
                self.hysteresis.record_trigger("vram_falling", now);
            }
        }

        // Trigger 3: KV-Cache wächst stark
        if snapshot.kv_cache_mb > self.config.kv_cache_threshold_mb {
            if self.hysteresis.can_trigger("kv_growing", now) {
                actions.push(TriggerAction::BudgetKV);
                actions.push(TriggerAction::EvictOptionalCache);
                self.hysteresis.record_trigger("kv_growing", now);
            }
        }

        // Trigger 4: MoE-Hit-Rate niedrig
        if self.moe_hit_rate < self.config.moe_hit_rate_threshold {
            if self.hysteresis.can_trigger("moe_low_hit", now) {
                actions.push(TriggerAction::LoadPredictedExperts);
                self.hysteresis.record_trigger("moe_low_hit", now);
            }
        }

        // Trigger 5: Prefetch-Genauigkeit niedrig
        if self.prefetch_accuracy < 0.5 {
            if self.hysteresis.can_trigger("prefetch_inaccurate", now) {
                actions.push(TriggerAction::DisableSpeculativePrefetch);
                self.hysteresis.record_trigger("prefetch_inaccurate", now);
            }
        }

        // Trigger 6: Queue hoch
        if snapshot.queue_depth > self.config.queue_depth_threshold {
            if self.hysteresis.can_trigger("queue_high", now) {
                actions.push(TriggerAction::BatchTransfers);
                self.hysteresis.record_trigger("queue_high", now);
            }
        }

        // Trigger 7: SSD-Read im Decode
        if snapshot.decode_in_progress && self.detect_ssd_read_in_decode(snapshot) {
            if self.hysteresis.can_trigger("ssd_in_decode", now) {
                actions.push(TriggerAction::AbortSSDRead);
                self.hysteresis.record_trigger("ssd_in_decode", now);
            }
        }

        // Trigger 8: Kontext fast voll
        if self.context_usage_percent > self.config.context_full_threshold_percent {
            if self.hysteresis.can_trigger("context_full", now) {
                actions.push(TriggerAction::CompressContext);
                self.hysteresis.record_trigger("context_full", now);
            }
        }

        actions
    }

    /// Erkennt ob SSD-Read während Decode aktiv ist
    fn detect_ssd_read_in_decode(&self, _snapshot: &ResourceSnapshot) -> bool {
        // TODO: Integration mit performance.rs Transfer-Samples
        // Für MVP: Immer false (kein SSD-Read im Decode)
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_snapshot(vram_used: u32, vram_total: u32) -> ResourceSnapshot {
        ResourceSnapshot {
            timestamp: Utc::now(),
            vram_used_mb: vram_used,
            vram_total_mb: vram_total,
            ram_used_mb: 16000,
            ram_total_mb: 32768,
            pinned_ram_used_mb: 0,
            pinned_ram_limit_mb: 4096,
            kv_cache_mb: 0,
            queue_depth: 0,
            cpu_usage_percent: 50.0,
            active_model_id: None,
            decode_in_progress: false,
        }
    }

    #[test]
    fn test_vram_critical_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);

        let snapshot = make_snapshot(10800, 11264); // 95.9% VRAM
        let actions = engine.evaluate(&snapshot);

        assert!(actions.contains(&TriggerAction::StopPrefetch));
        assert!(actions.contains(&TriggerAction::EvictColdRegion));
    }

    #[test]
    fn test_vram_falling_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);

        let snapshot = make_snapshot(9700, 11264); // 86.1% VRAM
        let actions = engine.evaluate(&snapshot);

        assert!(actions.contains(&TriggerAction::ReducePrefetch));
        assert!(actions.contains(&TriggerAction::ReplanResidency));
    }

    #[test]
    fn test_kv_cache_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);

        let mut snapshot = make_snapshot(5000, 11264);
        snapshot.kv_cache_mb = 5000; // Über 4096 MB Threshold
        let actions = engine.evaluate(&snapshot);

        assert!(actions.contains(&TriggerAction::BudgetKV));
        assert!(actions.contains(&TriggerAction::EvictOptionalCache));
    }

    #[test]
    fn test_moe_low_hit_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);
        engine.update_moe_hit_rate(0.4); // Unter 0.6 Threshold

        let snapshot = make_snapshot(5000, 11264);
        let actions = engine.evaluate(&snapshot);

        assert!(actions.contains(&TriggerAction::LoadPredictedExperts));
    }

    #[test]
    fn test_context_full_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);
        engine.update_context_usage(0.95); // Über 0.9 Threshold

        let snapshot = make_snapshot(5000, 11264);
        let actions = engine.evaluate(&snapshot);

        assert!(actions.contains(&TriggerAction::CompressContext));
    }

    #[test]
    fn test_hysteresis_prevents_rapid_trigger() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);

        let snapshot = make_snapshot(10800, 11264); // VRAM kritisch

        // Erster Trigger
        let actions1 = engine.evaluate(&snapshot);
        assert!(!actions1.is_empty());

        // Zweiter Trigger sofort danach → durch Hysterese blockiert
        let actions2 = engine.evaluate(&snapshot);
        assert!(actions2.is_empty());
    }

    #[test]
    fn test_no_trigger_on_normal_load() {
        let config = TriggerConfig::default();
        let mut engine = TriggerEngine::new(config);

        let snapshot = make_snapshot(5000, 11264); // 44.4% VRAM — alles normal
        let actions = engine.evaluate(&snapshot);

        assert!(actions.is_empty());
    }
}
