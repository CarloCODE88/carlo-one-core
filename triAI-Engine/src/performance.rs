//! Leichte Laufzeitmetriken für Pipeline- und Speichertransferentscheidungen.
//!
//! Die Typen sind absichtlich ohne I/O und ohne Threading. Ein Worker kann sie
//! pro Transfer oder Decode-Fenster befüllen und die aggregierten Werte an die
//! bestehende Observability-Schicht weitergeben.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferKind {
    SsdToRam,
    RamToPinned,
    PinnedToVram,
    VramToPinned,
    PinnedToRam,
    RamToSsd,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransferSample {
    pub kind: TransferKind,
    pub bytes: u64,
    pub elapsed_us: u64,
    pub queued_us: u64,
    pub stall_us: u64,
}

impl TransferSample {
    pub fn bandwidth_gbps(self) -> f64 {
        if self.elapsed_us == 0 {
            return 0.0;
        }
        self.bytes as f64 / self.elapsed_us as f64 / 1_000.0
    }

    pub fn payload_ratio(self) -> f64 {
        let total = self.elapsed_us.saturating_add(self.queued_us);
        if total == 0 {
            return 0.0;
        }
        self.elapsed_us as f64 / total as f64
    }

    pub fn predicted_us(self, bytes: u64) -> u64 {
        if self.bytes == 0 {
            return u64::MAX;
        }
        ((self.elapsed_us as u128 * bytes as u128) / self.bytes as u128).min(u64::MAX as u128)
            as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PipelineWindow {
    pub classify_us: u64,
    pub context_load_us: u64,
    pub decode_us: u64,
    pub tool_us: u64,
    pub finalize_us: u64,
    pub vram_free_mb: u64,
    pub usable_vram_mb: u64,
    pub kv_cache_mb: u64,
    pub h2d_bytes: u64,
    pub d2h_bytes: u64,
    pub ssd_bytes: u64,
}

impl PipelineWindow {
    pub fn total_us(self) -> u64 {
        self.classify_us
            .saturating_add(self.context_load_us)
            .saturating_add(self.decode_us)
            .saturating_add(self.tool_us)
            .saturating_add(self.finalize_us)
    }

    pub fn vram_pressure(self) -> f64 {
        if self.usable_vram_mb == 0 {
            return 1.0;
        }
        (1.0 - self.vram_free_mb as f64 / self.usable_vram_mb as f64).clamp(0.0, 1.0)
    }

    pub fn transfer_bytes_per_decode_us(self) -> f64 {
        if self.decode_us == 0 {
            return 0.0;
        }
        (self.h2d_bytes.saturating_add(self.d2h_bytes)) as f64 / self.decode_us as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferAction {
    Keep,
    PrefetchOneChunk,
    BatchTransfers,
    EvictColdRegion,
    DisableSsdDuringDecode,
    ProtectKvCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Unterhalb dieser freien VRAM-Menge wird einmalig evicted.
    pub vram_enter_mb: u64,
    /// Erst oberhalb dieser höheren Menge wird der Eviction-Zustand verlassen.
    pub vram_exit_mb: u64,
    pub cooldown_ms: u64,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            vram_enter_mb: 768,
            vram_exit_mb: 1_024,
            cooldown_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerDecision {
    pub action: TransferAction,
    pub triggered: bool,
    pub reason: &'static str,
}

/// Zustandsbehaftete, deterministische Schutzmatrix. Zeit wird als Messwert
/// übergeben, damit sie ohne Schlafen und ohne Hardware in Tests reproduzierbar
/// ist. Die Hysterese verhindert Umschalten an einer einzelnen Schwelle; der
/// Cooldown begrenzt wiederholte teure Aktionen.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TriggerController {
    config: TriggerConfig,
    evicting: bool,
    last_action_ms: Option<u64>,
}

impl TriggerController {
    pub fn new(config: TriggerConfig) -> Self {
        assert!(config.vram_exit_mb >= config.vram_enter_mb);
        Self {
            config,
            evicting: false,
            last_action_ms: None,
        }
    }

    pub fn evaluate(&mut self, window: PipelineWindow, now_ms: u64) -> TriggerDecision {
        if window.vram_free_mb <= self.config.vram_enter_mb {
            self.evicting = true;
        } else if window.vram_free_mb >= self.config.vram_exit_mb {
            self.evicting = false;
        }
        if self.evicting {
            let cooled = self
                .last_action_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= self.config.cooldown_ms);
            if cooled {
                self.last_action_ms = Some(now_ms);
                return TriggerDecision {
                    action: TransferAction::EvictColdRegion,
                    triggered: true,
                    reason: "vram_hysteresis_enter_or_repeat_after_cooldown",
                };
            }
            return TriggerDecision {
                action: TransferAction::Keep,
                triggered: false,
                reason: "vram_evict_cooldown",
            };
        }
        TriggerDecision {
            action: TransferAction::Keep,
            triggered: false,
            reason: "vram_inside_safe_hysteresis_band",
        }
    }
}

/// Deterministische Schnellentscheidung; das Mini-Modell darf sie beraten,
/// aber nicht die harten Schutzentscheidungen überstimmen.
pub fn recommend_action(
    window: PipelineWindow,
    transfer: Option<TransferSample>,
    vram_reserve_mb: u64,
) -> TransferAction {
    if window.vram_free_mb < vram_reserve_mb {
        return TransferAction::EvictColdRegion;
    }
    if window.ssd_bytes > 0 && window.decode_us > 0 {
        return TransferAction::DisableSsdDuringDecode;
    }
    if window.kv_cache_mb > window.usable_vram_mb.saturating_mul(35) / 100 {
        return TransferAction::ProtectKvCache;
    }
    let Some(sample) = transfer else {
        return TransferAction::Keep;
    };
    if sample.queued_us > sample.elapsed_us {
        return TransferAction::BatchTransfers;
    }
    if matches!(
        sample.kind,
        TransferKind::SsdToRam | TransferKind::RamToPinned
    ) && sample.payload_ratio() > 0.60
    {
        return TransferAction::PrefetchOneChunk;
    }
    TransferAction::Keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> PipelineWindow {
        PipelineWindow {
            classify_us: 100,
            context_load_us: 200,
            decode_us: 10_000,
            tool_us: 0,
            finalize_us: 100,
            vram_free_mb: 4_000,
            usable_vram_mb: 10_000,
            kv_cache_mb: 1_000,
            h2d_bytes: 1_000_000,
            d2h_bytes: 100_000,
            ssd_bytes: 0,
        }
    }

    #[test]
    fn quantifies_bandwidth_and_projected_cost() {
        let sample = TransferSample {
            kind: TransferKind::SsdToRam,
            bytes: 10_000_000,
            elapsed_us: 10_000,
            queued_us: 2_000,
            stall_us: 500,
        };
        assert!((sample.bandwidth_gbps() - 1.0).abs() < 0.001);
        assert!(sample.payload_ratio() > 0.8);
        assert_eq!(sample.predicted_us(20_000_000), 20_000);
    }

    #[test]
    fn hard_vram_guard_wins_over_other_advice() {
        let mut current = window();
        current.vram_free_mb = 500;
        assert_eq!(
            recommend_action(current, None, 1_000),
            TransferAction::EvictColdRegion
        );
    }

    #[test]
    fn decode_never_uses_ssd_synchronously() {
        let mut current = window();
        current.ssd_bytes = 4096;
        assert_eq!(
            recommend_action(current, None, 1_000),
            TransferAction::DisableSsdDuringDecode
        );
    }

    #[test]
    fn trigger_hysteresis_and_cooldown_prevent_thrashing() {
        let mut controller = TriggerController::new(TriggerConfig {
            vram_enter_mb: 700,
            vram_exit_mb: 1_000,
            cooldown_ms: 100,
        });
        let mut current = window();
        current.vram_free_mb = 650;
        assert!(controller.evaluate(current, 0).triggered);
        assert!(!controller.evaluate(current, 50).triggered);
        assert!(controller.evaluate(current, 100).triggered);

        current.vram_free_mb = 800;
        assert!(!controller.evaluate(current, 150).triggered);
        current.vram_free_mb = 1_100;
        assert_eq!(
            controller.evaluate(current, 200).reason,
            "vram_inside_safe_hysteresis_band"
        );
    }

    #[test]
    fn trigger_config_rejects_inverted_hysteresis() {
        let result = std::panic::catch_unwind(|| {
            TriggerController::new(TriggerConfig {
                vram_enter_mb: 2_000,
                vram_exit_mb: 1_000,
                cooldown_ms: 1,
            });
        });
        assert!(result.is_err());
    }
}
