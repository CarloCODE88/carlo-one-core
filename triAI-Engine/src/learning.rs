//! Optionaler, datensparsamer Learning-Store für Modell- und Tool-Trajektorien.
//!
//! Der Store ist absichtlich nicht Teil des harten Inferenzpfads. Er schreibt
//! nur, wenn er explizit aktiviert wurde, und nimmt standardmäßig abgeleitete
//! Flags statt Rohprompt/-code entgegen.

use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

pub const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
static LEARNING_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LearningRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub timestamp_ms: u128,
    pub prompt_hmac: String,
    pub flags: serde_json::Value,
    pub model: ModelContext,
    pub trajectory: Trajectory,
    pub execution: Option<CodeExecutionTrace>,
    pub resources: ResourceTrace,
    pub outcome: OutcomeTrace,
    pub relations: CrossModelRelations,
    pub privacy: PrivacyState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelContext {
    pub model_id: String,
    pub model_digest: String,
    pub family: Option<String>,
    pub architecture_kind: Option<String>,
    pub quantization: Option<String>,
    pub policy_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Trajectory {
    pub phases: Vec<PhaseTrace>,
    pub tool_calls: Vec<ToolCallTrace>,
    pub output_hash: Option<String>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub correction_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseTrace {
    pub name: String,
    pub duration_us: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolCallTrace {
    pub name: String,
    pub arguments_hash: Option<String>,
    pub result_class: Option<String>,
    pub duration_us: u64,
    pub exit_code: Option<i32>,
    pub changed_files: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeExecutionTrace {
    pub language: String,
    pub project_kind: Option<String>,
    pub command_class: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub stdout_hash: Option<String>,
    pub stderr_class: Option<String>,
    pub tests_total: Option<u32>,
    pub tests_failed: Option<u32>,
    pub lint_errors: Option<u32>,
    pub patch_hunks: Option<u32>,
    pub patch_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceTrace {
    pub first_token_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub vram_peak_mb: Option<u64>,
    pub ram_peak_mb: Option<u64>,
    pub pinned_ram_peak_mb: Option<u64>,
    pub ssd_bytes: u64,
    pub h2d_bytes: u64,
    pub d2h_bytes: u64,
    pub active_area_summary: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomeTrace {
    pub quality_score: Option<f64>,
    pub correctness_score: Option<f64>,
    pub user_acceptance: Option<bool>,
    pub retry_count: u32,
    pub regression_detected: bool,
    pub failure_classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CrossModelRelations {
    pub task_cluster_id: Option<String>,
    pub comparable_run_ids: Vec<String>,
    pub capability_labels: Vec<String>,
    pub disagreement_class: Option<String>,
    pub preferred_model: Option<String>,
    pub transfer_independent_signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrivacyState {
    pub raw_prompt_stored: bool,
    pub raw_code_stored: bool,
    pub raw_output_stored: bool,
    pub secret_scan_passed: bool,
    pub retention_class: String,
}

pub struct LearningStore {
    path: PathBuf,
    enabled: bool,
}

impl LearningStore {
    pub fn new(path: impl Into<PathBuf>, enabled: bool) -> Self {
        Self {
            path: path.into(),
            enabled,
        }
    }

    pub fn append(&self, record: &LearningRecord) -> io::Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        validate_record(record)?;
        let line = serde_json::to_vec(record).map_err(io::Error::other)?;
        if line.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "learning record exceeds size limit",
            ));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = LEARNING_WRITE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn validate_record(record: &LearningRecord) -> io::Result<()> {
    if record.schema_version == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "learning record requires a schema version",
        ));
    }
    if record.run_id.is_empty() || record.run_id.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "learning record requires a bounded run_id",
        ));
    }
    let privacy = &record.privacy;
    if privacy.raw_prompt_stored || privacy.raw_code_stored || privacy.raw_output_stored {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "raw prompt, code, and output are forbidden in learning evidence",
        ));
    }
    if !privacy.secret_scan_passed {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "learning evidence requires a successful secret scan",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_store_does_not_write() {
        let path = std::env::temp_dir().join(format!("tri-learning-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = LearningStore::new(&path, false);
        assert!(!store.append(&LearningRecord::default()).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn enabled_store_writes_parseable_record() {
        let path =
            std::env::temp_dir().join(format!("tri-learning-write-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = LearningStore::new(&path, true);
        let record = LearningRecord {
            schema_version: 1,
            run_id: "r1".into(),
            model: ModelContext {
                model_id: "mini".into(),
                ..Default::default()
            },
            privacy: PrivacyState {
                secret_scan_passed: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(store.append(&record).unwrap());
        let line = std::fs::read_to_string(&path).unwrap();
        let _: LearningRecord = serde_json::from_str(line.trim()).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enabled_store_rejects_raw_or_unscanned_evidence() {
        let path =
            std::env::temp_dir().join(format!("tri-learning-reject-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = LearningStore::new(&path, true);
        let record = LearningRecord {
            schema_version: 1,
            run_id: "r2".into(),
            privacy: PrivacyState {
                raw_prompt_stored: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            store.append(&record).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!path.exists());
    }
}
