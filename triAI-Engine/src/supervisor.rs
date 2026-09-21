use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt, unix::process::ExitStatusExt},
    process::{Child, ChildStderr, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[path = "supervisor/fallback.rs"]
pub mod fallback;
#[path = "supervisor/manifest.rs"]
pub mod manifest;
#[path = "supervisor/primary.rs"]
pub mod primary;
#[path = "supervisor/slot.rs"]
pub mod slot;
#[path = "supervisor/expert_tracker.rs"]
pub mod expert_tracker;

pub use fallback::{FailoverReason, MiniModelSupervisor};
pub use manifest::{MiniModelManifest, MiniModelSpec};
pub use primary::{Advice, AdviceDecision, AdviceFailure};
pub use expert_tracker::{ExpertEvent, ExpertTracker};

const STDERR_TAIL_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub binary: String,
    /// Stabile Kennung für API, Engine-Zustand und Chat-Anfragen.
    pub model: String,
    /// Tatsächlicher lokaler GGUF-Pfad. Fehlt nur in Legacy-/Stub-Tests;
    /// dann wird `model` als Worker-Argument verwendet.
    pub model_path: Option<String>,
    pub host: String,
    pub port: u16,
    pub gpu_layers: u32,
    pub ctx_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Stopped,
    Starting,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureCode {
    WorkerTimeout,
    WorkerCrashed,
    OutOfMemory,
    PortConflict,
    CorruptModel,
    SpawnFailed,
    SlotBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerFailure {
    pub code: WorkerFailureCode,
    pub message: String,
    /// Nur für lokale Diagnose; wird nicht automatisch in API oder Eventlog
    /// serialisiert und ist auf 64 KiB begrenzt.
    pub stderr_tail: String,
}

impl std::fmt::Display for WorkerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerFailure {}

pub struct WorkerSupervisor {
    child: Option<Child>,
    state: WorkerState,
    active_model: Option<String>,
    slot_lock: Option<File>,
    /// In-process guard complements the cross-process flock below.
    process_slot: Option<slot::SlotLease>,
    stderr_capture: Option<StderrCapture>,
    last_failure: Option<WorkerFailure>,
}

/// The single-model-slot lock always lives at this well-known path in
/// production. `start()` resolves the actual path through `slot_lock_path()`,
/// which lets tests redirect it to a process-unique file via the
/// `TRI_AI_RUNNER_TEST_LOCK_PATH` env var — this machine runs several
/// worktrees of this same crate in parallel, and without redirecting, their
/// `cargo test` runs would spuriously contend for this one shared file. The
/// mechanism (a `flock` on a well-known file) and the production path are
/// unchanged; only the path lookup is indirected.
const DEFAULT_SLOT_LOCK_PATH: &str = "/tmp/tri-ai-runner-single-model.lock";

fn slot_lock_path() -> std::path::PathBuf {
    std::env::var_os("TRI_AI_RUNNER_TEST_LOCK_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_SLOT_LOCK_PATH))
}

impl WorkerSupervisor {
    pub fn new() -> Self {
        Self {
            child: None,
            state: WorkerState::Stopped,
            active_model: None,
            slot_lock: None,
            process_slot: None,
            stderr_capture: None,
            last_failure: None,
        }
    }
    pub fn state(&self) -> WorkerState {
        self.state
    }
    pub fn active_model(&self) -> Option<&str> {
        self.active_model.as_deref()
    }
    pub fn last_failure(&self) -> Option<&WorkerFailure> {
        self.last_failure.as_ref()
    }

    pub fn start(&mut self, cfg: &WorkerConfig) -> io::Result<()> {
        if self.child.is_some() || self.active_model.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "single-model slot is busy",
            ));
        }
        self.process_slot = match slot::global().acquire(&cfg.model) {
            Ok(lease) => Some(lease),
            Err(error) => {
                self.state = WorkerState::Failed;
                self.last_failure = Some(WorkerFailure {
                    code: WorkerFailureCode::SlotBusy,
                    message: error.to_string(),
                    stderr_tail: String::new(),
                });
                return Err(error);
            }
        };
        // O_CLOEXEC is set atomically at open() (not via a separate fcntl()
        // afterwards) so a fork() racing on another thread can never inherit
        // this fd in the window between opening it and marking it
        // close-on-exec. Without this, the lock fd could leak across
        // fork+exec into a spawned child, which would keep the flock alive
        // even after our own copy is closed, since a `flock` is only fully
        // released once *every* fd referring to the same open file
        // description is closed.
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(slot_lock_path())?;
        let raw_fd = lock.as_raw_fd();
        // The previous holder of this slot (an old worker we — or a sibling
        // process — just stopped) releases its flock the instant its last fd
        // is closed, but that can trail the "stop" call by a hair under load.
        // A serial model switch (stop old -> start new) must not fail on
        // that race, so retry briefly before giving up.
        let mut locked = false;
        for attempt in 0..20 {
            if unsafe { libc::flock(raw_fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                locked = true;
                break;
            }
            if attempt < 19 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if !locked {
            self.process_slot = None;
            self.state = WorkerState::Failed;
            self.last_failure = Some(WorkerFailure {
                code: WorkerFailureCode::SlotBusy,
                message: "ein anderer TRI-AI-Runner besitzt den einzigen Modell-Slot".into(),
                stderr_tail: String::new(),
            });
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another TRI-AI runner owns the single-model slot",
            ));
        }
        self.slot_lock = Some(lock);
        self.state = WorkerState::Starting;
        let mut command = Command::new(&cfg.binary);
        // llama.cpp-Binaries enthalten häufig einen RUNPATH ihres
        // ursprünglichen Build-Verzeichnisses. Der TRI-AI-Vendor-Ordner ist
        // bewusst relocatable; deshalb steht sein Binary-Verzeichnis vor
        // einem bereits vorhandenen LD_LIBRARY_PATH.
        if let Some(dir) = std::path::Path::new(&cfg.binary).parent() {
            let mut paths = vec![dir.to_path_buf()];
            if let Some(existing) = env::var_os("LD_LIBRARY_PATH") {
                paths.extend(env::split_paths(&existing));
            }
            if let Ok(value) = env::join_paths(paths) {
                command.env("LD_LIBRARY_PATH", value);
            }
        }
        let result = command
            .args([
                "--model",
                cfg.model_path.as_deref().unwrap_or(&cfg.model),
                "--host",
                &cfg.host,
                "--port",
                &cfg.port.to_string(),
                "--gpu-layers",
                // llama.cpp needs one extra offload slot for the output
                // projection when the planner selected all model layers.
                // Keep the public plan value equal to the GGUF layer count;
                // this is only the backend command-line representation.
                &cfg.gpu_layers.saturating_add(1).to_string(),
                "--ctx-size",
                &cfg.ctx_size.to_string(),
                "--parallel",
                "1",
                // RTX 2080 Ti baseline: one interactive slot, GPU KV cache
                // and Flash Attention.  These match the measured Franz
                // runner baseline; context pressure is handled by the
                // planner before spawning rather than by overcommitting VRAM.
                "--flash-attn",
                "on",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
                "--threads",
                "8",
                "--threads-batch",
                "8",
                "--cpu-strict",
                "1",
                "--batch-size",
                "512",
                "--ubatch-size",
                "512",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        match result {
            Ok(mut child) => {
                let Some(stderr) = child.stderr.take() else {
                    terminate_child(&mut child);
                    self.slot_lock = None;
                    self.process_slot = None;
                    self.state = WorkerState::Failed;
                    self.last_failure = Some(WorkerFailure {
                        code: WorkerFailureCode::SpawnFailed,
                        message: "Worker-Stderr-Pipe wurde nicht angelegt".into(),
                        stderr_tail: String::new(),
                    });
                    return Err(io::Error::other("worker stderr pipe missing"));
                };
                self.stderr_capture = Some(StderrCapture::start(stderr));
                self.child = Some(child);
                self.active_model = Some(cfg.model.clone());
                self.last_failure = None;
                crate::observability::emit(
                    "worker_started",
                    serde_json::json!({
                        "model": cfg.model,
                        "port": cfg.port,
                        "gpu_layers": cfg.gpu_layers,
                        "ctx_size": cfg.ctx_size,
                    }),
                );
                Ok(())
            }
            Err(e) => {
                self.slot_lock = None;
                self.process_slot = None;
                self.state = WorkerState::Failed;
                self.last_failure = Some(WorkerFailure {
                    code: WorkerFailureCode::SpawnFailed,
                    message: format!("Worker-Prozess konnte nicht gestartet werden: {e}"),
                    stderr_tail: String::new(),
                });
                crate::observability::emit(
                    "worker_start_failed",
                    serde_json::json!({"model": cfg.model, "error_kind": format!("{:?}", e.kind())}),
                );
                Err(e)
            }
        }
    }

    /// Startet genau einen Worker und wartet synchron bis Readiness, Exit
    /// oder Deadline. Jeder Fehlerpfad beendet/reapt den Kindprozess und gibt
    /// den globalen Modell-Slot frei.
    pub fn start_and_wait(
        &mut self,
        cfg: &WorkerConfig,
        timeout: Duration,
    ) -> Result<(), WorkerFailure> {
        if tcp_ready(&cfg.host, cfg.port) {
            let failure = WorkerFailure {
                code: WorkerFailureCode::PortConflict,
                message: format!("Worker-Port {} ist bereits belegt", cfg.port),
                stderr_tail: String::new(),
            };
            self.state = WorkerState::Failed;
            self.last_failure = Some(failure.clone());
            return Err(failure);
        }
        self.start(cfg).map_err(|error| {
            self.last_failure.clone().unwrap_or_else(|| WorkerFailure {
                code: if error.kind() == io::ErrorKind::WouldBlock {
                    WorkerFailureCode::SlotBusy
                } else {
                    WorkerFailureCode::SpawnFailed
                },
                message: error.to_string(),
                stderr_tail: String::new(),
            })
        })?;

        let deadline = Instant::now() + timeout;
        loop {
            let Some(child) = self.child.as_mut() else {
                let stderr_tail = self.cleanup_failed_worker(false);
                let failure = WorkerFailure {
                    code: WorkerFailureCode::WorkerCrashed,
                    message: "Worker-Handle ging vor Readiness verloren".into(),
                    stderr_tail,
                };
                self.last_failure = Some(failure.clone());
                return Err(failure);
            };
            let status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let stderr_tail = self.cleanup_failed_worker(true);
                    let failure = WorkerFailure {
                        code: WorkerFailureCode::WorkerCrashed,
                        message: format!("Worker-Status nicht lesbar: {error}"),
                        stderr_tail,
                    };
                    self.last_failure = Some(failure.clone());
                    return Err(failure);
                }
            };
            if let Some(status) = status {
                let stderr_tail = self.cleanup_failed_worker(false);
                let failure = classify_exit(status, stderr_tail);
                self.last_failure = Some(failure.clone());
                return Err(failure);
            }
            if tcp_ready(&cfg.host, cfg.port) {
                // llama.cpp binds its socket before the model/KV cache is
                // usable and returns HTTP 503 during that interval. Do not
                // expose that startup race as an inference-ready engine.
                if worker_health_ready(&cfg.host, cfg.port) == Some(false) {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                self.state = WorkerState::Ready;
                return Ok(());
            }
            if Instant::now() >= deadline {
                let stderr_tail = self.cleanup_failed_worker(true);
                let failure = WorkerFailure {
                    code: WorkerFailureCode::WorkerTimeout,
                    message: format!(
                        "Worker wurde innerhalb von {} ms nicht bereit",
                        timeout.as_millis()
                    ),
                    stderr_tail,
                };
                self.last_failure = Some(failure.clone());
                return Err(failure);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Genau ein Retry ist nur dann erlaubt, wenn ein Worker einen klaren
    /// Bind-Konflikt meldete, vollständig bereinigt wurde und der Port danach
    /// nachweislich wieder frei ist.
    pub fn start_and_wait_with_retry(
        &mut self,
        cfg: &WorkerConfig,
        timeout: Duration,
    ) -> Result<(), WorkerFailure> {
        match self.start_and_wait(cfg, timeout) {
            Err(first)
                if first.code == WorkerFailureCode::PortConflict
                    && !tcp_ready(&cfg.host, cfg.port) =>
            {
                self.state = WorkerState::Stopped;
                self.last_failure = None;
                self.start_and_wait(cfg, timeout)
            }
            other => other,
        }
    }

    fn cleanup_failed_worker(&mut self, terminate: bool) -> String {
        if let Some(mut child) = self.child.take() {
            if terminate {
                terminate_child(&mut child);
            } else {
                let _ = child.wait();
            }
        }
        let stderr_tail = self
            .stderr_capture
            .take()
            .map(StderrCapture::finish)
            .unwrap_or_default();
        self.active_model = None;
        self.slot_lock = None;
        self.process_slot = None;
        self.state = WorkerState::Failed;
        stderr_tail
    }

    pub fn refresh(&mut self, host: &str, port: u16) -> WorkerState {
        let Some(child) = self.child.as_mut() else {
            self.state = WorkerState::Stopped;
            return self.state;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                // The worker process exited on its own (crash, OOM-kill,
                // etc.). Release the single-model slot lock here too, not
                // just in `stop()` — otherwise it stays held by this
                // now-dead worker's already-closed fd forever, and every
                // subsequent `start()` fails with "slot busy" even though
                // nothing is actually running anymore.
                let stderr_tail = self.cleanup_failed_worker(false);
                self.last_failure = Some(classify_exit(status, stderr_tail));
            }
            Ok(None) => {
                if tcp_ready(host, port) {
                    self.state = WorkerState::Ready;
                }
            }
            Err(_) => self.state = WorkerState::Failed,
        }
        self.state
    }

    pub fn stop(&mut self) -> io::Result<()> {
        if let Some(mut child) = self.child.take() {
            terminate_child(&mut child);
        }
        if let Some(capture) = self.stderr_capture.take() {
            let _ = capture.finish();
        }
        self.active_model = None;
        self.slot_lock = None;
        self.process_slot = None;
        self.state = WorkerState::Stopped;
        self.last_failure = None;
        crate::observability::emit("worker_stopped", serde_json::json!({}));
        Ok(())
    }
}

impl Default for WorkerSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

struct StderrCapture {
    tail: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl StderrCapture {
    fn start(mut stderr: ChildStderr) -> Self {
        let fd = stderr.as_raw_fd();
        // SAFETY: fcntl liest/setzt ausschließlich die Statusflags des
        // gültigen, von ChildStderr gehaltenen Dateideskriptors.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        let tail = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_tail = tail.clone();
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut tail = reader_tail
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        append_tail(&mut tail, &chunk[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if reader_stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            tail,
            stop,
            reader: Some(reader),
        }
    }

    fn finish(mut self) -> String {
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let tail = self
            .tail
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        String::from_utf8_lossy(&tail).into_owned()
    }
}

fn append_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= STDERR_TAIL_LIMIT {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - STDERR_TAIL_LIMIT..]);
        return;
    }
    let overflow = tail
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(STDERR_TAIL_LIMIT);
    if overflow > 0 {
        tail.drain(..overflow);
    }
    tail.extend_from_slice(bytes);
}

fn classify_exit(status: ExitStatus, stderr_tail: String) -> WorkerFailure {
    let lower = stderr_tail.to_ascii_lowercase();
    let oom = status.code() == Some(137)
        || status.signal() == Some(libc::SIGKILL)
        || [
            "out of memory",
            "cuda_error_out_of_memory",
            "failed to allocate",
            "cannot allocate memory",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
    let port_conflict = ["address already in use", "failed to bind", "bind: address"]
        .iter()
        .any(|marker| lower.contains(marker));
    let corrupt_model = [
        "invalid gguf",
        "failed to load model",
        "unknown model architecture",
        "invalid model file",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    let code = if oom {
        WorkerFailureCode::OutOfMemory
    } else if port_conflict {
        WorkerFailureCode::PortConflict
    } else if corrupt_model {
        WorkerFailureCode::CorruptModel
    } else {
        WorkerFailureCode::WorkerCrashed
    };
    WorkerFailure {
        code,
        message: format!("Worker wurde vor Readiness beendet ({status})"),
        stderr_tail,
    }
}

/// Grace period allotted to the child after SIGTERM before escalating to
/// SIGKILL, split into small polling steps so we return as soon as the
/// process exits on its own instead of always sleeping the full budget.
const GRACEFUL_STOP_POLL_ATTEMPTS: u32 = 20;
const GRACEFUL_STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Attempts a graceful shutdown of `child`, escalating to SIGKILL if it
/// doesn't exit in time, and always reaps it before returning.
///
/// `llama-server` gets a chance to flush open files and release its GPU
/// context on SIGTERM; `Child::kill()` alone always sends SIGKILL on Unix
/// and gives it no such chance.
fn terminate_child(child: &mut Child) {
    // SAFETY: `libc::kill` with a valid pid and a standard signal number is
    // always safe to call; a failure (e.g. ESRCH because the process already
    // exited on its own) is reported via the return value, not UB, and is
    // handled below by simply proceeding to poll/reap.
    let sigterm_sent = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } == 0;

    let mut exited = false;
    if sigterm_sent {
        for attempt in 0..GRACEFUL_STOP_POLL_ATTEMPTS {
            match child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => {
                    if attempt + 1 < GRACEFUL_STOP_POLL_ATTEMPTS {
                        std::thread::sleep(GRACEFUL_STOP_POLL_INTERVAL);
                    }
                }
                Err(_) => break,
            }
        }
    }

    if !exited {
        // Either SIGTERM couldn't be delivered, or the process ignored it /
        // didn't exit within the grace period: escalate.
        let _ = child.kill();
    }
    // Always reap. On Linux, `std::process::Child` caches the exit status
    // internally the first time `wait()`/`try_wait()` observes it, so
    // calling `wait()` here even after `try_wait()` already reaped the
    // process above is safe and simply returns the cached status — no
    // double-wait/ECHILD hazard.
    let _ = child.wait();
}

impl Drop for WorkerSupervisor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn tcp_ready(host: &str, port: u16) -> bool {
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(150)).ok())
        .is_some()
}

/// Returns Some(true/false) for a readable HTTP health response, or None for
/// TCP-only test workers and legacy workers that do not expose `/health`.
fn worker_health_ready(host: &str, port: u16) -> Option<bool> {
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(150)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).ok()?;
    health_status_ready(&buf[..n])
}

fn health_status_ready(response: &[u8]) -> Option<bool> {
    let line = response.split(|byte| *byte == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let mut parts = text.split_whitespace();
    let version = parts.next()?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return None;
    }
    match parts.next()?.parse::<u16>().ok()? {
        200..=299 => Some(true),
        500..=599 => Some(false),
        _ => None,
    }
}

// Serializes tests that touch the real single-model-slot lock and gives each
// *call* its own dedicated file (see `slot_lock_path`) instead of the shared
// production path. Two independent reasons this needs to be per-call, not
// just per-process: (1) sibling `cargo test` runs in other worktrees of this
// same crate on this machine must never contend with ours on the shared
// production path, and (2) even within one process, an OS `flock` denies a
// conflicting lock request from *any* still-open file descriptor referring
// to the same file — including another one opened earlier by this very
// process — so unrelated guarded tests must not share a filename either,
// only a test that deliberately wants two of its own supervisors to contend
// with each other (see `second_supervisor_cannot_acquire_occupied_slot`)
// should ever reuse the same call's path.
#[cfg(test)]
pub(crate) static SLOT_LOCK_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static SLOT_LOCK_TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn acquire_test_lock_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = SLOT_LOCK_TEST_GUARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let call_id = SLOT_LOCK_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::set_var(
        "TRI_AI_RUNNER_TEST_LOCK_PATH",
        std::env::temp_dir().join(format!(
            "tri-ai-runner-single-model-test-{}-{}.lock",
            std::process::id(),
            call_id
        )),
    );
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn health_probe_accepts_ready_and_rejects_loading_status() {
        assert_eq!(health_status_ready(b"HTTP/1.1 200 OK\r\n"), Some(true));
        assert_eq!(
            health_status_ready(b"HTTP/1.1 503 Service Unavailable\r\n"),
            Some(false)
        );
        assert_eq!(health_status_ready(b"not-http"), None);
    }

    /// Writes a `#!/bin/sh` stub script (ignoring whatever args a real
    /// `WorkerConfig` would pass it) to a fresh temp file and marks it
    /// executable, so it can be used directly as `WorkerConfig::binary`.
    fn write_stub_script(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tri-ai-runner-stub-{}-{}-{}.sh",
            std::process::id(),
            name,
            SLOT_LOCK_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut f = File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn stub_config(binary: &std::path::Path, port: u16) -> WorkerConfig {
        WorkerConfig {
            binary: binary.to_string_lossy().into_owned(),
            model: "fixture.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port,
            gpu_layers: 0,
            ctx_size: 4096,
        }
    }

    #[test]
    fn new_supervisor_is_stopped() {
        assert_eq!(WorkerSupervisor::new().state(), WorkerState::Stopped);
    }
    #[test]
    fn missing_binary_fails_without_child() {
        let _guard = acquire_test_lock_guard();
        let mut s = WorkerSupervisor::new();
        let c = WorkerConfig {
            binary: "/missing/llama-server".into(),
            model: "m.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19001,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        assert!(s.start(&c).is_err());
        assert_eq!(s.state(), WorkerState::Failed);
    }
    #[test]
    fn second_supervisor_cannot_acquire_occupied_slot() {
        let _guard = acquire_test_lock_guard();
        let cfg = WorkerConfig {
            binary: "/bin/sleep".into(),
            model: "5".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19011,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        let mut first = WorkerSupervisor::new();
        first.start(&cfg).unwrap();
        assert_ne!(first.state(), WorkerState::Failed);

        let mut second = WorkerSupervisor::new();
        let err = second.start(&cfg);
        assert!(err.is_err());
        assert_eq!(second.state(), WorkerState::Failed);
        // No second child process was ever spawned for the occupied slot.
        assert!(second.child.is_none());

        first.stop().unwrap();
    }
    #[test]
    fn refresh_after_worker_crash_releases_slot_lock() {
        let _guard = acquire_test_lock_guard();
        let cfg = WorkerConfig {
            binary: "/bin/false".into(),
            model: "m.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19013,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        let mut s = WorkerSupervisor::new();
        s.start(&cfg).unwrap();

        // `/bin/false` ignores all args and exits immediately, simulating a
        // worker that crashed right after starting. Poll refresh() (as a
        // real caller would) until it observes the exit.
        let mut state = WorkerState::Starting;
        for _ in 0..100 {
            state = s.refresh(&cfg.host, cfg.port);
            if state == WorkerState::Failed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(state, WorkerState::Failed);

        // The crash must have released the single-model slot lock, not just
        // the child handle — otherwise every future start() would wrongly
        // report the slot as busy forever.
        let mut s2 = WorkerSupervisor::new();
        assert!(s2.start(&cfg).is_ok());
        s2.stop().unwrap();
    }

    #[test]
    fn stop_uses_sigterm_and_returns_quickly_for_cooperative_worker() {
        let _guard = acquire_test_lock_guard();
        // `sleep` is backgrounded and joined via the `wait` builtin rather
        // than run directly in the foreground: on this shell, a trap for a
        // signal delivered to a foreground *external* command only runs
        // after that command finishes on its own (verified empirically —
        // `trap 'exit 0' TERM; sleep 5` took the full 5s even after SIGTERM
        // arrived early). `wait`, in contrast, is interrupted immediately
        // by a trapped signal, which is what a real cooperative shutdown
        // handler needs.
        let script = write_stub_script("cooperative", "trap 'exit 0' TERM\nsleep 5 &\nwait\n");
        let cfg = WorkerConfig {
            binary: script.to_string_lossy().into_owned(),
            model: "m.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19021,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        let mut s = WorkerSupervisor::new();
        s.start(&cfg).unwrap();
        // Give the shell a moment to install the trap before we signal it.
        std::thread::sleep(Duration::from_millis(50));

        let before = std::time::Instant::now();
        s.stop().unwrap();
        let elapsed = before.elapsed();

        // A cooperative worker reacts to SIGTERM almost instantly, so this
        // must come back far faster than the full ~500ms SIGKILL escalation
        // budget (GRACEFUL_STOP_POLL_ATTEMPTS * GRACEFUL_STOP_POLL_INTERVAL)
        // — proving SIGTERM was actually used instead of jumping straight to
        // SIGKILL.
        assert!(
            elapsed < Duration::from_millis(400),
            "stop() took {elapsed:?}, expected a fast graceful exit"
        );
        assert_eq!(s.state(), WorkerState::Stopped);
        assert!(s.child.is_none());

        // The process must be genuinely gone (no zombie, lock released):
        // starting a fresh worker on the same slot must succeed.
        let mut s2 = WorkerSupervisor::new();
        assert!(s2.start(&cfg).is_ok());
        s2.stop().unwrap();

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn stop_escalates_to_sigkill_when_worker_ignores_sigterm() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script("stubborn", "trap '' TERM\nsleep 5 &\nwait\n");
        let cfg = WorkerConfig {
            binary: script.to_string_lossy().into_owned(),
            model: "m.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19022,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        let mut s = WorkerSupervisor::new();
        s.start(&cfg).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let before = std::time::Instant::now();
        s.stop().unwrap();
        let elapsed = before.elapsed();

        // Must have waited out (roughly) the full grace period before
        // escalating, but must still come back well before the test would
        // time out, proving SIGKILL escalation actually happened rather
        // than stop() hanging forever.
        assert!(
            elapsed >= Duration::from_millis(400),
            "stop() returned after only {elapsed:?}, expected it to wait out the grace period first"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "stop() took {elapsed:?}, expected SIGKILL escalation to finish quickly"
        );
        assert_eq!(s.state(), WorkerState::Stopped);
        assert!(s.child.is_none());

        // The relevant proof the process is truly dead (not just detached):
        // a brand-new supervisor can immediately reacquire the same slot.
        let mut s2 = WorkerSupervisor::new();
        assert!(s2.start(&cfg).is_ok());
        s2.stop().unwrap();

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn stop_is_idempotent_and_safe_to_call_again_after_child_already_reaped() {
        let _guard = acquire_test_lock_guard();
        let cfg = WorkerConfig {
            binary: "/bin/sleep".into(),
            model: "5".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: 19023,
            gpu_layers: 0,
            ctx_size: 4096,
        };
        let mut s = WorkerSupervisor::new();
        s.start(&cfg).unwrap();

        // First stop() sends SIGTERM, polls, and reaps the child.
        s.stop().unwrap();
        assert_eq!(s.state(), WorkerState::Stopped);

        // Calling stop() again with no child present must stay a safe
        // no-op (covers the "double wait" concern at the WorkerSupervisor
        // level: nothing left to signal or reap, and no panic/error).
        assert!(s.stop().is_ok());
        assert_eq!(s.state(), WorkerState::Stopped);
    }

    #[test]
    fn readiness_timeout_reaps_child_and_releases_slot() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script("timeout", "sleep 5\n");
        let cfg = stub_config(&script, 19131);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait(&cfg, Duration::from_millis(30))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::WorkerTimeout);
        assert_eq!(supervisor.state(), WorkerState::Failed);
        assert!(supervisor.child.is_none());
        assert!(supervisor.slot_lock.is_none());

        let mut next = WorkerSupervisor::new();
        assert!(next.start(&cfg).is_ok());
        next.stop().unwrap();
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn crash_is_classified_and_stderr_is_captured() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script("crash", "echo 'plain crash marker' >&2\nexit 2\n");
        let cfg = stub_config(&script, 19132);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait(&cfg, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::WorkerCrashed);
        assert!(failure.stderr_tail.contains("plain crash marker"));
        assert!(supervisor.child.is_none());
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn clear_oom_marker_is_classified_without_retry() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script(
            "oom",
            "echo 'CUDA_ERROR_OUT_OF_MEMORY while allocating buffer' >&2\nexit 1\n",
        );
        let cfg = stub_config(&script, 19133);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait_with_retry(&cfg, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::OutOfMemory);
        assert!(supervisor.child.is_none());
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn clear_model_load_marker_is_classified_as_corrupt_model() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script(
            "corrupt",
            "echo 'failed to load model: invalid GGUF' >&2\nexit 1\n",
        );
        let cfg = stub_config(&script, 19134);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait(&cfg, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::CorruptModel);
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn occupied_port_is_rejected_before_spawn() {
        let _guard = acquire_test_lock_guard();
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("Loopback-Port konnte nicht gebunden werden: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let script = write_stub_script("port-preflight", "sleep 5\n");
        let cfg = stub_config(&script, port);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait(&cfg, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::PortConflict);
        assert!(supervisor.child.is_none());
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn stderr_tail_is_bounded_and_keeps_latest_bytes() {
        let _guard = acquire_test_lock_guard();
        let script = write_stub_script(
            "stderr-tail",
            "dd if=/dev/zero bs=70000 count=1 2>/dev/null | tr '\\000' x >&2\necho 'FINAL-TAIL' >&2\nexit 3\n",
        );
        let cfg = stub_config(&script, 19135);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait(&cfg, Duration::from_secs(2))
            .unwrap_err();
        assert!(failure.stderr_tail.len() <= STDERR_TAIL_LIMIT);
        assert!(failure.stderr_tail.contains("FINAL-TAIL"));
        let _ = std::fs::remove_file(script);
    }

    #[test]
    fn transient_bind_failure_retries_exactly_once_after_cleanup() {
        let _guard = acquire_test_lock_guard();
        let counter = std::env::temp_dir().join(format!(
            "tri-ai-retry-counter-{}-{}",
            std::process::id(),
            SLOT_LOCK_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let body = format!(
            "count=0\n[ -f '{0}' ] && count=$(sed -n '1p' '{0}')\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > '{0}'\necho 'bind: address already in use' >&2\nexit 1\n",
            counter.display()
        );
        let script = write_stub_script("retry-once", &body);
        let cfg = stub_config(&script, 19136);
        let mut supervisor = WorkerSupervisor::new();
        let failure = supervisor
            .start_and_wait_with_retry(&cfg, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(failure.code, WorkerFailureCode::PortConflict);
        assert_eq!(std::fs::read_to_string(&counter).unwrap().trim(), "2");
        assert!(supervisor.child.is_none());
        let _ = std::fs::remove_file(script);
        let _ = std::fs::remove_file(counter);
    }
}
