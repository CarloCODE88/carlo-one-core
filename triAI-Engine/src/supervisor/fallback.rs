//! Deterministic primary/fallback selection. This module never starts two workers.

use super::{
    manifest::{MiniModelManifest, MiniModelSpec},
    primary::AdviceFailure,
    WorkerConfig, WorkerFailure, WorkerFailureCode,
};
use crate::engine::{Engine, EngineState};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveMiniModel {
    Primary,
    Fallback,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    PrimaryNotReady,
    AdviceTimeout,
    InvalidAdvice,
    VramPressure,
}

#[derive(Debug, Clone)]
pub struct MiniRuntimeConfig {
    pub binary: String,
    pub host: String,
    pub port: u16,
    pub gpu_layers: u32,
    pub ctx_size: u32,
    pub readiness_timeout: Duration,
}

pub struct MiniModelSupervisor {
    manifest: MiniModelManifest,
    active: ActiveMiniModel,
    failover_count: u64,
}

impl MiniModelSupervisor {
    pub fn new(manifest: MiniModelManifest) -> Self {
        Self {
            manifest,
            active: ActiveMiniModel::Primary,
            failover_count: 0,
        }
    }
    pub fn active(&self) -> ActiveMiniModel {
        self.active
    }
    pub fn failover_count(&self) -> u64 {
        self.failover_count
    }
    pub fn active_spec(&self) -> &MiniModelSpec {
        match self.active {
            ActiveMiniModel::Primary => &self.manifest.primary_mini_model,
            ActiveMiniModel::Fallback => &self.manifest.fallback_mini_model,
        }
    }
    pub fn fail_over(&mut self, reason: FailoverReason) -> &MiniModelSpec {
        self.active = ActiveMiniModel::Fallback;
        self.failover_count = self.failover_count.saturating_add(1);
        crate::observability::emit(
            "mini_model_failover",
            serde_json::json!({"reason": format!("{reason:?}"), "count": self.failover_count}),
        );
        self.active_spec()
    }
    pub fn apply_advice_failure(&mut self, failure: AdviceFailure) -> Option<FailoverReason> {
        let reason = match failure {
            AdviceFailure::PrimaryUnavailable => FailoverReason::PrimaryNotReady,
            AdviceFailure::Timeout => FailoverReason::AdviceTimeout,
            AdviceFailure::InvalidJson | AdviceFailure::InvalidSchema => {
                FailoverReason::InvalidAdvice
            }
            AdviceFailure::VramPressure => FailoverReason::VramPressure,
        };
        self.fail_over(reason);
        Some(reason)
    }

    /// Starts exactly one verified mini model. A failed primary receives one
    /// serial fallback attempt; the existing engine slot-lock remains owner.
    pub fn ensure_ready(
        &mut self,
        engine: &mut Engine,
        project_root: &Path,
        runtime: &MiniRuntimeConfig,
        vram_pressure: bool,
    ) -> Result<ActiveMiniModel, WorkerFailure> {
        if vram_pressure && self.active != ActiveMiniModel::Fallback {
            self.fail_over(FailoverReason::VramPressure);
        }
        match self.start_selected(engine, project_root, runtime) {
            Ok(active) => Ok(active),
            Err(_) if self.active == ActiveMiniModel::Primary => {
                self.fail_over(FailoverReason::PrimaryNotReady);
                self.start_selected(engine, project_root, runtime)
            }
            Err(error) => Err(error),
        }
    }

    fn start_selected(
        &self,
        engine: &mut Engine,
        project_root: &Path,
        runtime: &MiniRuntimeConfig,
    ) -> Result<ActiveMiniModel, WorkerFailure> {
        let spec = self.active_spec();
        spec.verify_digest(project_root)
            .map_err(|error| WorkerFailure {
                code: WorkerFailureCode::CorruptModel,
                message: error.to_string(),
                stderr_tail: String::new(),
            })?;
        if engine.active_model() == Some(spec.id.as_str()) && engine.state() == EngineState::Ready {
            return Ok(self.active);
        }
        let cfg = worker_config(spec, project_root, runtime).map_err(|error| WorkerFailure {
            code: WorkerFailureCode::CorruptModel,
            message: error.to_string(),
            stderr_tail: String::new(),
        })?;
        if engine.state() == EngineState::Ready {
            engine.stop_model().map_err(|error| WorkerFailure {
                code: WorkerFailureCode::SlotBusy,
                message: error.to_string(),
                stderr_tail: String::new(),
            })?;
            engine.start_model_ready(&cfg, runtime.readiness_timeout)?;
        } else if engine.state() == EngineState::Idle {
            engine.start_model_ready(&cfg, runtime.readiness_timeout)?;
        } else if matches!(engine.state(), EngineState::Degraded | EngineState::Failed) {
            engine.recover_with_fallback(&cfg, runtime.readiness_timeout)?;
        } else {
            return Err(WorkerFailure {
                code: WorkerFailureCode::SlotBusy,
                message: "mini-model switch requires an idle or failed engine".into(),
                stderr_tail: String::new(),
            });
        }
        Ok(self.active)
    }
}

pub fn worker_config(
    spec: &MiniModelSpec,
    project_root: &Path,
    runtime: &MiniRuntimeConfig,
) -> crate::error::Result<WorkerConfig> {
    let path: PathBuf = spec.resolved_path(project_root)?;
    Ok(WorkerConfig {
        binary: runtime.binary.clone(),
        model: spec.id.clone(),
        model_path: Some(path.to_string_lossy().into_owned()),
        host: runtime.host.clone(),
        port: runtime.port,
        gpu_layers: runtime.gpu_layers,
        ctx_size: runtime.ctx_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> MiniModelManifest {
        let item = |id: &str, hash: &str| MiniModelSpec {
            id: id.into(),
            alias: None,
            path: "models/x.gguf".into(),
            quantization: "Q4".into(),
            size_bytes: 1,
            sha256: hash.into(),
            role: "advice".into(),
            policy: "safe".into(),
        };
        MiniModelManifest {
            schema_version: 1,
            primary_mini_model: item("primary", &"a".repeat(64)),
            fallback_mini_model: item("fallback", &"b".repeat(64)),
        }
    }
    #[test]
    fn timeout_switches_to_fallback_once_per_failure() {
        let mut supervisor = MiniModelSupervisor::new(manifest());
        assert_eq!(supervisor.active(), ActiveMiniModel::Primary);
        assert_eq!(
            supervisor.apply_advice_failure(AdviceFailure::Timeout),
            Some(FailoverReason::AdviceTimeout)
        );
        assert_eq!(supervisor.active(), ActiveMiniModel::Fallback);
        assert_eq!(supervisor.failover_count(), 1);
    }

    #[test]
    fn worker_config_uses_manifest_path_not_model_identifier() {
        let root = tempfile::TempDir::new().unwrap();
        let spec = MiniModelSpec {
            id: "logical-id".into(),
            alias: None,
            path: "models/model.gguf".into(),
            quantization: "Q4".into(),
            size_bytes: 1,
            sha256: "a".repeat(64),
            role: "advice".into(),
            policy: "safe".into(),
        };
        let runtime = MiniRuntimeConfig {
            binary: "llama-server".into(),
            host: "127.0.0.1".into(),
            port: 8901,
            gpu_layers: 1,
            ctx_size: 512,
            readiness_timeout: Duration::from_secs(2),
        };
        let cfg = worker_config(&spec, root.path(), &runtime).unwrap();
        assert_eq!(cfg.model, "logical-id");
        assert_eq!(
            cfg.model_path,
            Some(
                root.path()
                    .join("models/model.gguf")
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }
}
