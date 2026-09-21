//! Hysterese und Cooldowns verhindern Thrashing.
//!
//! Jeder Trigger-Typ hat einen eigenen Cooldown-Timer.
//! Exponential Backoff bei wiederholten Triggern.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cooldown-Konfiguration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CooldownConfig {
    pub base_cooldown_ms: u64,   // 5000ms Basis-Cooldown
    pub max_cooldown_ms: u64,    // 60000ms maximaler Cooldown
    pub backoff_multiplier: f64, // 2.0x bei jedem Repeat
    pub reset_after_ms: u64,     // 300000ms ohne Trigger → Reset
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            base_cooldown_ms: 5000,
            max_cooldown_ms: 60000,
            backoff_multiplier: 2.0,
            reset_after_ms: 300_000,
        }
    }
}

/// Trigger-State mit Cooldown-Tracking
#[derive(Debug, Clone)]
struct TriggerState {
    last_triggered: Instant,
    consecutive_triggers: u32,
    current_cooldown_ms: u64,
}

/// Hysterese-State für alle Trigger-Typen
pub struct HysteresisState {
    config: CooldownConfig,
    states: HashMap<String, TriggerState>,
}

impl HysteresisState {
    pub fn new(config: CooldownConfig) -> Self {
        Self {
            config,
            states: HashMap::new(),
        }
    }

    /// Prüft ob ein Trigger ausgelöst werden darf
    pub fn can_trigger(&self, trigger_type: &str, now: Instant) -> bool {
        if let Some(state) = self.states.get(trigger_type) {
            let elapsed = now.duration_since(state.last_triggered);
            let cooldown = Duration::from_millis(state.current_cooldown_ms);
            elapsed >= cooldown
        } else {
            // Erster Trigger: Immer erlaubt
            true
        }
    }

    /// Registriert einen ausgelösten Trigger
    pub fn record_trigger(&mut self, trigger_type: &str, now: Instant) {
        let base_cooldown = self.config.base_cooldown_ms;
        let state = self
            .states
            .entry(trigger_type.to_string())
            .or_insert_with(|| TriggerState {
                last_triggered: now,
                consecutive_triggers: 0,
                current_cooldown_ms: base_cooldown,
            });

        state.last_triggered = now;
        state.consecutive_triggers += 1;

        // Exponential Backoff
        let new_cooldown =
            (state.current_cooldown_ms as f64 * self.config.backoff_multiplier) as u64;
        state.current_cooldown_ms = new_cooldown.min(self.config.max_cooldown_ms);
    }

    /// Setzt Cooldown zurück (wenn Trigger lange nicht ausgelöst wurde)
    pub fn reset_if_stale(&mut self, trigger_type: &str, now: Instant) {
        if let Some(state) = self.states.get_mut(trigger_type) {
            let elapsed = now.duration_since(state.last_triggered);
            let reset_threshold = Duration::from_millis(self.config.reset_after_ms);

            if elapsed >= reset_threshold {
                state.consecutive_triggers = 0;
                state.current_cooldown_ms = self.config.base_cooldown_ms;
            }
        }
    }

    /// Gibt aktuelle Cooldown-Info für Debugging
    pub fn get_cooldown_info(&self, trigger_type: &str) -> Option<(u64, u32)> {
        self.states
            .get(trigger_type)
            .map(|s| (s.current_cooldown_ms, s.consecutive_triggers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_trigger_always_allowed() {
        let config = CooldownConfig::default();
        let hysteresis = HysteresisState::new(config);
        let now = Instant::now();

        assert!(hysteresis.can_trigger("test", now));
    }

    #[test]
    fn test_cooldown_blocks_rapid_trigger() {
        let config = CooldownConfig {
            base_cooldown_ms: 1000,
            max_cooldown_ms: 5000,
            backoff_multiplier: 2.0,
            reset_after_ms: 10000,
        };
        let mut hysteresis = HysteresisState::new(config);
        let now = Instant::now();

        // Erster Trigger
        assert!(hysteresis.can_trigger("test", now));
        hysteresis.record_trigger("test", now);

        // Sofort danach: Blockiert
        assert!(!hysteresis.can_trigger("test", now));

        // Nach Cooldown: Erlaubt
        let later = now + Duration::from_millis(2100);
        assert!(hysteresis.can_trigger("test", later));
    }

    #[test]
    fn test_exponential_backoff() {
        let config = CooldownConfig {
            base_cooldown_ms: 1000,
            max_cooldown_ms: 8000,
            backoff_multiplier: 2.0,
            reset_after_ms: 10000,
        };
        let mut hysteresis = HysteresisState::new(config);
        let mut now = Instant::now();

        // Trigger 1: base → 1000 * 2.0 = 2000ms
        hysteresis.record_trigger("test", now);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 2000);

        // Trigger 2: 2000 * 2.0 = 4000ms
        now += Duration::from_millis(2100);
        hysteresis.record_trigger("test", now);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 4000);

        // Trigger 3: 4000 * 2.0 = 8000ms
        now += Duration::from_millis(4100);
        hysteresis.record_trigger("test", now);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 8000);

        // Trigger 4: capped at max 8000ms
        now += Duration::from_millis(8100);
        hysteresis.record_trigger("test", now);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 8000);
    }

    #[test]
    fn test_reset_after_stale() {
        let config = CooldownConfig {
            base_cooldown_ms: 1000,
            max_cooldown_ms: 8000,
            backoff_multiplier: 2.0,
            reset_after_ms: 5000,
        };
        let mut hysteresis = HysteresisState::new(config);
        let now = Instant::now();

        // Trigger mit Backoff
        hysteresis.record_trigger("test", now);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 2000);

        // Nach Reset-Threshold: Cooldown zurücksetzen
        let later = now + Duration::from_millis(6000);
        hysteresis.reset_if_stale("test", later);
        assert_eq!(hysteresis.states["test"].current_cooldown_ms, 1000);
        assert_eq!(hysteresis.states["test"].consecutive_triggers, 0);
    }

    #[test]
    fn test_get_cooldown_info() {
        let config = CooldownConfig::default();
        let mut hysteresis = HysteresisState::new(config);
        let now = Instant::now();

        assert!(hysteresis.get_cooldown_info("unknown").is_none());

        hysteresis.record_trigger("test", now);
        let info = hysteresis.get_cooldown_info("test").unwrap();
        assert_eq!(info.0, 10000); // 5000 * 2.0
        assert_eq!(info.1, 1);
    }
}
