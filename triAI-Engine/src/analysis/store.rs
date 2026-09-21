//! Metric-only persistence for policy proposals.

use super::policy::PolicyProposal;
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub struct MetricPolicyStore {
    path: PathBuf,
    enabled: bool,
}

impl MetricPolicyStore {
    pub fn new(path: impl Into<PathBuf>, enabled: bool) -> Self {
        Self {
            path: path.into(),
            enabled,
        }
    }
    pub fn append(&self, proposal: &PolicyProposal) -> io::Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        if !proposal.confidence.is_finite()
            || !proposal.disk_savings_percent.is_finite()
            || !proposal.decode_change_percent.is_finite()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "policy metrics must be finite",
            ));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_vec(proposal).map_err(io::Error::other)?;
        let _guard = WRITE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.write_all(b"\n")?;
        Ok(true)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::policy::PolicyProposal;
    #[test]
    fn metric_store_persists_only_structured_numeric_proposals() {
        let path = std::env::temp_dir().join(format!("triai-policy-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let proposal = PolicyProposal {
            version: 1,
            source_window_start_ms: 1,
            source_window_end_ms: 2,
            cache_budget_mb: 512,
            prefetch_depth: 1,
            confidence: 0.9,
            disk_savings_percent: 20.0,
            decode_change_percent: 0.0,
        };
        assert!(MetricPolicyStore::new(&path, true)
            .append(&proposal)
            .unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("prompt"));
        let _: PolicyProposal = serde_json::from_str(content.trim()).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn store_rejects_non_finite_metrics_and_disabled_store_writes_nothing() {
        let path =
            std::env::temp_dir().join(format!("triai-policy-invalid-{}.jsonl", std::process::id()));
        let invalid = PolicyProposal {
            version: 1,
            source_window_start_ms: 1,
            source_window_end_ms: 2,
            cache_budget_mb: 512,
            prefetch_depth: 1,
            confidence: f64::NAN,
            disk_savings_percent: 20.0,
            decode_change_percent: 0.0,
        };
        assert!(MetricPolicyStore::new(&path, true)
            .append(&invalid)
            .is_err());
        let valid = PolicyProposal {
            confidence: 0.9,
            ..invalid.clone()
        };
        assert!(!MetricPolicyStore::new(&path, false).append(&valid).unwrap());
        assert!(!path.exists());
    }
}
