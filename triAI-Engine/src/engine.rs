use crate::{
    staging::{CommitRecord, StageStore},
    supervisor::{WorkerConfig, WorkerFailure, WorkerState, WorkerSupervisor},
};
use std::{io, path::PathBuf, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Idle,
    Loading,
    Ready,
    Busy,
    /// Worker ist ausgefallen; ein Recovery-/Fallback-Versuch ist möglich.
    Degraded,
    /// Aktive Requests werden vor einem kontrollierten Wechsel beendet.
    Draining,
    /// Ein Wiederanlauf oder Rollback auf eine bekannte Konfiguration läuft.
    Rollback,
    Stopping,
    Failed,
}

pub struct Engine {
    supervisor: WorkerSupervisor,
    staging: StageStore,
    state: EngineState,
    active_model: Option<String>,
    worker_endpoint: Option<(String, u16)>,
    last_config: Option<WorkerConfig>,
}

impl Engine {
    pub fn new(stage_root: impl Into<PathBuf>) -> io::Result<Self> {
        let staging = StageStore::new(stage_root)?;
        staging.cleanup_incomplete()?;
        Ok(Self {
            supervisor: WorkerSupervisor::new(),
            staging,
            state: EngineState::Idle,
            active_model: None,
            worker_endpoint: None,
            last_config: None,
        })
    }
    pub fn state(&self) -> EngineState {
        self.state
    }
    pub fn active_model(&self) -> Option<&str> {
        self.active_model.as_deref()
    }
    pub fn worker_endpoint(&self) -> Option<(&str, u16)> {
        self.worker_endpoint
            .as_ref()
            .map(|(host, port)| (host.as_str(), *port))
    }
    pub fn stage_block(&self, id: &str, generation: u64, data: &[u8]) -> io::Result<CommitRecord> {
        self.staging.stage_bytes(id, generation, data)
    }
    pub fn recover_staging(&self) -> io::Result<Vec<CommitRecord>> {
        self.staging.recover()
    }

    pub fn start_model(&mut self, cfg: &WorkerConfig) -> io::Result<()> {
        if self.active_model.is_some() || self.state != EngineState::Idle {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "engine single-model slot is busy",
            ));
        }
        self.state = EngineState::Loading;
        match self.supervisor.start(cfg) {
            Ok(()) => {
                self.active_model = Some(cfg.model.clone());
                self.worker_endpoint = Some((cfg.host.clone(), cfg.port));
                self.state = EngineState::Loading;
                self.last_config = Some(cfg.clone());
                Ok(())
            }
            Err(e) => {
                self.state = EngineState::Failed;
                Err(e)
            }
        }
    }

    pub fn start_model_ready(
        &mut self,
        cfg: &WorkerConfig,
        timeout: Duration,
    ) -> Result<(), WorkerFailure> {
        if self.active_model.is_some() || self.state != EngineState::Idle {
            return Err(WorkerFailure {
                code: crate::supervisor::WorkerFailureCode::SlotBusy,
                message: "engine single-model slot is busy".into(),
                stderr_tail: String::new(),
            });
        }
        self.state = EngineState::Loading;
        match self.supervisor.start_and_wait_with_retry(cfg, timeout) {
            Ok(()) => {
                self.active_model = Some(cfg.model.clone());
                self.worker_endpoint = Some((cfg.host.clone(), cfg.port));
                self.state = EngineState::Ready;
                self.last_config = Some(cfg.clone());
                Ok(())
            }
            Err(error) => {
                self.active_model = None;
                self.worker_endpoint = None;
                self.state = EngineState::Failed;
                Err(error)
            }
        }
    }

    pub fn refresh(&mut self) -> EngineState {
        let Some((host, port)) = self.worker_endpoint.clone() else {
            return self.state;
        };
        let worker = self.supervisor.refresh(&host, port);
        match worker {
            WorkerState::Ready => {
                self.state = EngineState::Ready;
            }
            WorkerState::Failed => {
                self.state = EngineState::Degraded;
                self.active_model = None;
                self.worker_endpoint = None;
            }
            WorkerState::Stopped => {
                if self.state != EngineState::Stopping {
                    self.state = EngineState::Idle;
                    self.active_model = None;
                    self.worker_endpoint = None;
                }
            }
            WorkerState::Starting => {}
        }
        self.state
    }

    pub fn begin_request(&mut self, model: &str) -> io::Result<()> {
        if self.state != EngineState::Ready || self.active_model.as_deref() != Some(model) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "model is not the ready active model",
            ));
        }
        self.state = EngineState::Busy;
        Ok(())
    }
    pub fn finish_request(&mut self) -> io::Result<()> {
        if self.state != EngineState::Busy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no request active",
            ));
        }
        self.state = EngineState::Ready;
        Ok(())
    }
    pub fn stop_model(&mut self) -> io::Result<()> {
        if self.state == EngineState::Busy {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "active request must finish first",
            ));
        }
        self.state = EngineState::Draining;
        self.state = EngineState::Stopping;
        if let Err(error) = self.supervisor.stop() {
            self.state = EngineState::Failed;
            return Err(error);
        }
        self.active_model = None;
        self.worker_endpoint = None;
        self.last_config = None;
        self.state = EngineState::Idle;
        Ok(())
    }

    /// Startet eine explizit übergebene Fallback-Konfiguration nach einem
    /// Worker-Ausfall. Die Entscheidung, welches Fallback erlaubt ist, bleibt
    /// beim aufrufenden Control-Pfad; diese Methode führt nur einen bereits
    /// validierten Recovery-Versuch aus.
    pub fn recover_with_fallback(
        &mut self,
        cfg: &WorkerConfig,
        timeout: Duration,
    ) -> Result<(), WorkerFailure> {
        if !matches!(self.state, EngineState::Degraded | EngineState::Failed) {
            return Err(WorkerFailure {
                code: crate::supervisor::WorkerFailureCode::SlotBusy,
                message: "Fallback ist nur aus degraded/failed erlaubt".into(),
                stderr_tail: String::new(),
            });
        }
        self.state = EngineState::Rollback;
        self.active_model = None;
        self.worker_endpoint = None;
        match self.supervisor.start_and_wait_with_retry(cfg, timeout) {
            Ok(()) => {
                self.active_model = Some(cfg.model.clone());
                self.worker_endpoint = Some((cfg.host.clone(), cfg.port));
                self.last_config = Some(cfg.clone());
                self.state = EngineState::Ready;
                crate::observability::emit(
                    "worker_fallback_ready",
                    serde_json::json!({"model": cfg.model, "reason": "recovery"}),
                );
                Ok(())
            }
            Err(error) => {
                self.state = EngineState::Failed;
                crate::observability::emit(
                    "worker_fallback_failed",
                    serde_json::json!({"model": cfg.model, "code": error.code}),
                );
                Err(error)
            }
        }
    }

    /// Serially replace the currently active model with `cfg`.
    ///
    /// Rejects immediately (without touching the worker) if a request is in
    /// flight (`Busy`). Otherwise any existing worker is fully stopped and
    /// reaped via `stop_model()` — including releasing the single-model slot
    /// lock and `wait()`ing on the old child — before `start_model()` spawns
    /// the new one. Because the old child is fully reaped before the new one
    /// is spawned, two worker child processes can never exist at the same
    /// time; this is a structural property of the ordering, not just a state
    /// flag.
    pub fn switch_model(&mut self, cfg: &WorkerConfig) -> io::Result<()> {
        if self.state == EngineState::Busy {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot switch model while a request is active",
            ));
        }
        if self.active_model.is_some() || self.state != EngineState::Idle {
            self.stop_model()?;
        }
        self.start_model(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cfg() -> WorkerConfig {
        WorkerConfig {
            binary: "/missing/llama-server".into(),
            model: "a.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19101,
            gpu_layers: 0,
            // Updated to match default context size
            ctx_size: 4096,
        }
    }
    #[test]
    fn starts_idle_and_rejects_second_model() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let d = std::env::temp_dir().join(format!("tri-engine-{}", std::process::id()));
        let mut e = Engine::new(&d).unwrap();
        assert_eq!(e.state(), EngineState::Idle);
        assert!(e.start_model(&cfg()).is_err());
        assert_eq!(e.state(), EngineState::Failed);
        let _ = std::fs::remove_dir_all(d);
    }
    #[test]
    fn request_requires_ready_model() {
        let d = std::env::temp_dir().join(format!("tri-engine-req-{}", std::process::id()));
        let mut e = Engine::new(&d).unwrap();
        assert!(e.begin_request("a.gguf").is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn switch_model_rejected_while_busy_worker_untouched() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let d = std::env::temp_dir().join(format!("tri-engine-switch-busy-{}", std::process::id()));
        let mut e = Engine::new(&d).unwrap();
        let c1 = WorkerConfig {
            binary: "/bin/sleep".into(),
            model: "a.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19105,
            gpu_layers: 0,
            // Updated to match default context size
            ctx_size: 4096,
        };
        e.start_model(&c1).unwrap();
        // Force the engine into Ready/Busy without a real health-check poll,
        // simulating an in-flight request against the active worker.
        e.state = EngineState::Ready;
        e.begin_request(&c1.model).unwrap();
        assert_eq!(e.state(), EngineState::Busy);
        let worker_state_before = e.supervisor.state();

        let c2 = WorkerConfig {
            model: "b.gguf".into(),
            port: 19106,
            ..c1.clone()
        };
        let err = e.switch_model(&c2);
        assert!(err.is_err());
        // State, active model and the underlying worker must be completely
        // untouched by the rejected switch attempt.
        assert_eq!(e.state(), EngineState::Busy);
        assert_eq!(e.active_model(), Some("a.gguf"));
        assert_eq!(e.supervisor.state(), worker_state_before);

        e.finish_request().unwrap();
        e.stop_model().unwrap();
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn switch_model_stops_old_before_starting_new() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let d = std::env::temp_dir().join(format!("tri-engine-switch-ok-{}", std::process::id()));
        let mut e = Engine::new(&d).unwrap();
        let c1 = WorkerConfig {
            binary: "/bin/sleep".into(),
            model: "a.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19103,
            gpu_layers: 0,
            // Updated to match default context size
            ctx_size: 4096,
        };
        e.start_model(&c1).unwrap();
        assert_eq!(e.active_model(), Some("a.gguf"));

        let c2 = WorkerConfig {
            model: "b.gguf".into(),
            port: 19104,
            ..c1.clone()
        };
        e.switch_model(&c2).unwrap();
        assert_eq!(e.active_model(), Some("b.gguf"));
        assert_eq!(e.state(), EngineState::Loading);

        e.stop_model().unwrap();
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn failed_worker_enters_explicit_rollback_state_before_failing_recovery() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let d = std::env::temp_dir().join(format!("tri-engine-rollback-{}", std::process::id()));
        let mut engine = Engine::new(&d).unwrap();
        let failed = cfg();
        assert!(engine
            .start_model_ready(&failed, Duration::from_millis(20))
            .is_err());
        assert_eq!(engine.state(), EngineState::Failed);

        let fallback = WorkerConfig {
            model: "fallback.gguf".into(),
            ..failed
        };
        assert!(engine
            .recover_with_fallback(&fallback, Duration::from_millis(20))
            .is_err());
        assert_eq!(engine.state(), EngineState::Failed);
        let _ = std::fs::remove_dir_all(d);
    }
}
