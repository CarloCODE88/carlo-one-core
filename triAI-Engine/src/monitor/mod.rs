//! Live-Monitoring und Trigger-Engine für triAI-Engine.
//!
//! Erfasst alle 100ms Resource-Snapshots, wertet Trigger aus,
//! und gibt Policy-/Transfer-Entscheidungen ohne Decode-Blockierung.

pub mod hysteresis;
pub mod snapshot;
pub mod trigger;

pub use hysteresis::{CooldownConfig, HysteresisState};
pub use snapshot::{ResourceSnapshot, SystemMonitor};
pub use trigger::{TriggerAction, TriggerConfig, TriggerEngine};
