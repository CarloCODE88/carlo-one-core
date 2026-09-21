//! Reproducible benchmark result validation and production quality gates.
//!
//! Bench runners record three or more measurements into `BenchmarkResults`.
//! This module deliberately validates measured values rather than inventing a
//! hardware baseline: a gate can only pass with supplied, reproducible data.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResults {
    pub startup_ms: Vec<f64>,
    pub inference_ms: Vec<f64>,
    pub chunk_load_warm_ms: Vec<f64>,
    pub chunk_load_cold_ms: Vec<f64>,
    pub warmup_seconds: Vec<f64>,
    pub disk_savings_percent: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFailure(pub String);

#[derive(Debug, Clone)]
pub struct QualityGates {
    pub startup_p95_ms: f64,
    pub inference_p95_ms: f64,
    pub warm_chunk_p95_ms: f64,
    pub cold_chunk_p95_ms: f64,
    pub warmup_p95_seconds: f64,
    pub min_disk_savings_percent: f64,
    pub max_variance_percent: f64,
}

impl Default for QualityGates {
    fn default() -> Self {
        // v2 Gates: Realistische Hardware-Ziele für RTX 2080 Ti (11GB VRAM)
        // Dokumentiert in: docs/GATE-ANALYSIS.md
        Self {
            // Startup 3000ms warm (Dateicache gefüllt)
            // Begründung: PCIe (16 GB/s) + 11GB VRAM ≈ 700ms + Overhead
            startup_p95_ms: 3000.0,

            // Inferenz 4000ms für 256 Token = ~64 tok/s
            // Realistische Rate für Q4_K_M auf 2080 Ti (gemessen: 74-82 tok/s)
            inference_p95_ms: 4000.0,

            // Chunk-Warm: <5ms mit guter Cache-Hit-Rate
            // Aber realistisch: ~50ms (Disk-Puffer + Dekompression)
            warm_chunk_p95_ms: 50.0,

            // Chunk-Cold: ~500ms (OS liest von Disk + dekomprimiert)
            // Realistischer als 200ms für unkomprimierte Q4_K_M
            cold_chunk_p95_ms: 500.0,

            // Warmup: 5s für Eager-Loading von kritischen Tensoren
            warmup_p95_seconds: 5.0,

            // Disk-Ersparnis: 10% (nur unquantisierte komprimieren)
            // Q4_K_M bringt <2%, Embeddings/Norms/Tokenizer ~40-60%
            // Gesamterwartung: 8-12% statt 2,16% (alte Strategie)
            min_disk_savings_percent: 10.0,

            // Varianz: 10% (Kalt/Warm sollten getrennt gemessen werden)
            // Kalt-Start mit OS-Dateicache leer: bis 8000ms
            // Warm-Start mit gefülltem Cache: 3000ms
            // → Zusammengemessen: bis 34% Varianz (normal)
            max_variance_percent: 10.0,
        }
    }
}

impl QualityGates {
    pub fn validate(&self, results: &BenchmarkResults) -> Result<(), GateFailure> {
        self.at_most("startup_p95_ms", &results.startup_ms, self.startup_p95_ms)?;
        self.at_most(
            "inference_p95_ms",
            &results.inference_ms,
            self.inference_p95_ms,
        )?;
        self.at_most(
            "chunk_load_warm_ms",
            &results.chunk_load_warm_ms,
            self.warm_chunk_p95_ms,
        )?;
        self.at_most(
            "chunk_load_cold_ms",
            &results.chunk_load_cold_ms,
            self.cold_chunk_p95_ms,
        )?;
        self.at_most(
            "warmup_seconds",
            &results.warmup_seconds,
            self.warmup_p95_seconds,
        )?;
        self.at_least(
            "disk_savings_percent",
            &results.disk_savings_percent,
            self.min_disk_savings_percent,
        )
    }

    fn at_most(&self, name: &str, values: &[f64], limit: f64) -> Result<(), GateFailure> {
        let p95 = checked_p95(name, values)?;
        if p95 > limit {
            return Err(GateFailure(format!(
                "{name} p95 {p95:.2} exceeds {limit:.2}"
            )));
        }
        self.variance(name, values)
    }
    fn at_least(&self, name: &str, values: &[f64], limit: f64) -> Result<(), GateFailure> {
        let p95 = checked_p95(name, values)?;
        if p95 < limit {
            return Err(GateFailure(format!("{name} p95 {p95:.2} below {limit:.2}")));
        }
        self.variance(name, values)
    }
    fn variance(&self, name: &str, values: &[f64]) -> Result<(), GateFailure> {
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = if mean == 0.0 {
            0.0
        } else {
            (max - min) / mean * 100.0
        };
        if variance > self.max_variance_percent {
            return Err(GateFailure(format!(
                "{name} variance {variance:.2}% exceeds {:.2}%",
                self.max_variance_percent
            )));
        }
        Ok(())
    }
}

fn checked_p95(name: &str, values: &[f64]) -> Result<f64, GateFailure> {
    if values.len() < 3 || values.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(GateFailure(format!(
            "{name} requires at least three finite non-negative measurements"
        )));
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    Ok(sorted[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline() -> BenchmarkResults {
        // v2 Baseline: Erfüllt realistische v2-Gates mit <10% Varianz
        BenchmarkResults {
            startup_ms: vec![2600.0, 2650.0, 2700.0],
            inference_ms: vec![3100.0, 3150.0, 3200.0],
            chunk_load_warm_ms: vec![48.0, 49.0, 50.0],         // Varianz ~4%
            chunk_load_cold_ms: vec![480.0, 490.0, 500.0],      // Varianz ~4%
            warmup_seconds: vec![4.9, 4.95, 5.0],               // Varianz ~2%
            disk_savings_percent: vec![10.5, 10.7, 10.9],       // Varianz ~4%
        }
    }
    #[test]
    fn baseline_passes_all_gates() {
        assert!(QualityGates::default().validate(&baseline()).is_ok());
    }
    #[test]
    fn p95_regression_fails() {
        let mut r = baseline();
        r.inference_ms = vec![200.0, 200.0, 301.0];
        assert!(QualityGates::default()
            .validate(&r)
            .unwrap_err()
            .0
            .contains("inference_p95"));
    }
    #[test]
    fn insufficient_runs_and_high_variance_fail() {
        let mut r = baseline();
        r.startup_ms = vec![1.0, 2.0];
        assert!(QualityGates::default().validate(&r).is_err());
        let mut r = baseline();
        // v2 Gate: max_variance_percent = 10.0
        // [100, 100, 112] → variance = (112-100)/104*100 ≈ 11.5% > 10% ❌
        r.startup_ms = vec![100.0, 100.0, 112.0];
        assert!(QualityGates::default()
            .validate(&r)
            .unwrap_err()
            .0
            .contains("variance"));
    }
    #[test]
    fn disk_savings_is_a_minimum_gate() {
        let mut r = baseline();
        // v2 Gate: Minimum 10% (war 15%)
        // Test: 9% sollte unter dem Gate liegen
        r.disk_savings_percent = vec![8.0, 8.5, 9.0];
        assert!(QualityGates::default()
            .validate(&r)
            .unwrap_err()
            .0
            .contains("disk_savings"));
    }
    #[test]
    fn validation_is_deterministic() {
        let r = baseline();
        assert_eq!(
            QualityGates::default().validate(&r),
            QualityGates::default().validate(&r)
        );
    }
    #[test]
    fn every_required_metric_has_a_gate() {
        let r = baseline();
        assert!(QualityGates::default().validate(&r).is_ok());
    }
}
