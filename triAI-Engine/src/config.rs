//! Zentrale, typisierte Runner-Konfiguration.
//!
//! Die TOML-Datei liefert die Basis. Bestehende `TRI_AI_*`-Variablen werden
//! anschließend angewendet und haben dadurch stets Vorrang.

use serde::Deserialize;
use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG_PATH: &str = "tri-ai-runner.toml";

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub paths: PathConfig,
    pub worker: WorkerConfig,
    pub reserves: ReserveConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub inference_timeout_secs: u64,
    pub auth_token: Option<String>,
    /// Server-owned identity for evidence and budget attribution. Never read
    /// from an HTTP request body or header.
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathConfig {
    pub project_root: PathBuf,
    pub models_dir: Option<PathBuf>,
    pub ollama_root: Option<PathBuf>,
    pub lm_studio_roots: Vec<PathBuf>,
    pub jan_roots: Vec<PathBuf>,
    pub stage_dir: PathBuf,
    pub llama_server: PathBuf,
    pub event_log: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    pub port: u16,
    pub readiness_timeout_secs: u64,
    pub default_context: u32,
    pub default_gpu_layers: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReserveConfig {
    pub vram_mb: u64,
    pub ram_mb: u64,
    pub ssd_mb: u64,
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidEnv {
        name: &'static str,
        value: String,
    },
    InvalidValue(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "Konfiguration '{}' nicht lesbar: {source}",
                    path.display()
                )
            }
            Self::Toml { path, source } => {
                write!(
                    f,
                    "Konfiguration '{}' ist ungültig: {source}",
                    path.display()
                )
            }
            Self::InvalidEnv { name, value } => {
                write!(f, "Umgebungsvariable {name} hat ungültigen Wert '{value}'")
            }
            Self::InvalidValue(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8900".into(),
            inference_timeout_secs: 120,
            auth_token: None,
            user_id: "local-system".into(),
        }
    }
}

impl Default for PathConfig {
    fn default() -> Self {
        Self {
            project_root: ".".into(),
            models_dir: None,
            ollama_root: None,
            lm_studio_roots: Vec::new(),
            jan_roots: Vec::new(),
            stage_dir: "./staging".into(),
            llama_server: "vendor/bin/llama-cpp/llama-server".into(),
            event_log: "tri-ai-events.jsonl".into(),
        }
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            port: 8901,
            readiness_timeout_secs: 60,
            default_context: 4096,
            default_gpu_layers: 999,
        }
    }
}

impl Default for ReserveConfig {
    fn default() -> Self {
        Self {
            vram_mb: 1024,
            ram_mb: 2048,
            ssd_mb: 5120,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let explicit_path = env::var_os("TRI_AI_CONFIG").map(PathBuf::from);
        let path = explicit_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let current_dir = env::current_dir().map_err(|source| ConfigError::Io {
            path: PathBuf::from("."),
            source,
        })?;
        let absolute_path = absolutize(&current_dir, &path);
        let config_base = absolute_path.parent().unwrap_or(&current_dir);
        let mut config = match fs::read_to_string(&absolute_path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Toml {
                path: absolute_path.clone(),
                source,
            })?,
            Err(source) if source.kind() == io::ErrorKind::NotFound && explicit_path.is_none() => {
                Self::default()
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    path: absolute_path,
                    source,
                });
            }
        };
        config.apply_env()?;
        config.resolve_paths(config_base);
        config.validate()?;
        Ok(config)
    }

    pub fn models_dir(&self) -> PathBuf {
        self.paths
            .models_dir
            .clone()
            .unwrap_or_else(|| self.paths.project_root.join("models"))
    }

    fn apply_env(&mut self) -> Result<(), ConfigError> {
        override_string("TRI_AI_LISTEN_ADDR", &mut self.server.listen_addr);
        override_optional_string("TRI_AI_AUTH_TOKEN", &mut self.server.auth_token);
        override_string("TRI_AI_USER_ID", &mut self.server.user_id);
        override_path("TRI_AI_PROJECT_ROOT", &mut self.paths.project_root);
        override_optional_path("TRI_AI_MODELS_DIR", &mut self.paths.models_dir);
        override_optional_path("TRI_AI_OLLAMA_ROOT", &mut self.paths.ollama_root);
        override_paths("TRI_AI_LM_STUDIO_ROOTS", &mut self.paths.lm_studio_roots);
        override_paths("TRI_AI_JAN_ROOTS", &mut self.paths.jan_roots);
        override_path("TRI_AI_STAGE_DIR", &mut self.paths.stage_dir);
        override_path("TRI_AI_LLAMA_SERVER", &mut self.paths.llama_server);
        override_path("TRI_AI_EVENT_LOG", &mut self.paths.event_log);
        override_number(
            "TRI_AI_INFERENCE_TIMEOUT_SECS",
            &mut self.server.inference_timeout_secs,
        )?;
        override_number("TRI_AI_WORKER_PORT", &mut self.worker.port)?;
        override_number(
            "TRI_AI_WORKER_READINESS_TIMEOUT_SECS",
            &mut self.worker.readiness_timeout_secs,
        )?;
        override_number("TRI_AI_DEFAULT_CONTEXT", &mut self.worker.default_context)?;
        override_number(
            "TRI_AI_DEFAULT_GPU_LAYERS",
            &mut self.worker.default_gpu_layers,
        )?;
        override_number("TRI_AI_VRAM_RESERVE_MB", &mut self.reserves.vram_mb)?;
        override_number("TRI_AI_RAM_RESERVE_MB", &mut self.reserves.ram_mb)?;
        override_number("TRI_AI_SSD_RESERVE_MB", &mut self.reserves.ssd_mb)?;
        Ok(())
    }

    fn validate(&mut self) -> Result<(), ConfigError> {
        if self.server.listen_addr.trim().is_empty() {
            return Err(ConfigError::InvalidValue(
                "server.listen_addr darf nicht leer sein".into(),
            ));
        }
        if !is_loopback_bind(&self.server.listen_addr)
            && self.server.auth_token.as_deref().is_none_or(str::is_empty)
        {
            return Err(ConfigError::InvalidValue(
                "externe Bind-Adresse benoetigt TRI_AI_AUTH_TOKEN oder server.auth_token".into(),
            ));
        }
        if self.server.user_id.is_empty()
            || self.server.user_id.len() > 128
            || !self
                .server
                .user_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ConfigError::InvalidValue(
                "server.user_id darf nur ASCII-Buchstaben, Ziffern, '-' und '_' enthalten".into(),
            ));
        }
        if self.worker.port == 0 {
            return Err(ConfigError::InvalidValue(
                "worker.port muss zwischen 1 und 65535 liegen".into(),
            ));
        }
        self.server.inference_timeout_secs = self.server.inference_timeout_secs.clamp(1, 600);
        self.worker.readiness_timeout_secs = self.worker.readiness_timeout_secs.clamp(1, 300);
        self.worker.default_context = self.worker.default_context.max(512);
        Ok(())
    }

    /// Relative Pfade sind eindeutig: `project_root` relativ zur
    /// Konfigurationsdatei, alle übrigen Pfade relativ zum Projektwurzelpfad.
    fn resolve_paths(&mut self, config_base: &Path) {
        self.paths.project_root = absolutize(config_base, &self.paths.project_root);
        let project_root = self.paths.project_root.clone();
        self.paths.models_dir = self
            .paths
            .models_dir
            .take()
            .map(|path| absolutize(&project_root, &path));
        self.paths.ollama_root = self
            .paths
            .ollama_root
            .take()
            .map(|path| absolutize(&project_root, &path));
        resolve_path_list(&project_root, &mut self.paths.lm_studio_roots);
        resolve_path_list(&project_root, &mut self.paths.jan_roots);
        self.paths.stage_dir = absolutize(&project_root, &self.paths.stage_dir);
        self.paths.llama_server = absolutize(&project_root, &self.paths.llama_server);
        self.paths.event_log = absolutize(&project_root, &self.paths.event_log);
    }
}

fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn resolve_path_list(base: &Path, paths: &mut [PathBuf]) {
    for path in paths {
        *path = absolutize(base, path);
    }
}

fn override_string(name: &'static str, target: &mut String) {
    if let Ok(value) = env::var(name) {
        *target = value;
    }
}

fn override_optional_string(name: &'static str, target: &mut Option<String>) {
    if let Ok(value) = env::var(name) {
        *target = Some(value);
    }
}

fn is_loopback_bind(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(host, _)| host).unwrap_or(addr);
    let host = host.trim_matches(['[', ']']);
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn override_path(name: &'static str, target: &mut PathBuf) {
    if let Some(value) = env::var_os(name) {
        *target = value.into();
    }
}

fn override_optional_path(name: &'static str, target: &mut Option<PathBuf>) {
    if let Some(value) = env::var_os(name) {
        *target = Some(value.into());
    }
}

fn override_paths(name: &'static str, target: &mut Vec<PathBuf>) {
    if let Some(value) = env::var_os(name) {
        *target = env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
    }
}

fn override_number<T>(name: &'static str, target: &mut T) -> Result<(), ConfigError>
where
    T: std::str::FromStr,
{
    let Ok(value) = env::var(name) else {
        return Ok(());
    };
    *target = value.parse().map_err(|_| ConfigError::InvalidEnv {
        name,
        value: value.clone(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn temp_config(contents: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "tri-ai-config-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn loads_toml_and_derives_models_directory() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = temp_config(
            r#"
                [paths]
                project_root = "/srv/tri-ai"

                [worker]
                port = 9901
            "#,
        );
        env::set_var("TRI_AI_CONFIG", &path);
        let config = Config::load().unwrap();
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();
        assert_eq!(config.worker.port, 9901);
        assert_eq!(config.models_dir(), PathBuf::from("/srv/tri-ai/models"));
        assert_eq!(config.server.listen_addr, "127.0.0.1:8900");
    }

    #[test]
    fn environment_overrides_toml() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = temp_config("[worker]\nport = 9901\n");
        env::set_var("TRI_AI_CONFIG", &path);
        env::set_var("TRI_AI_WORKER_PORT", "9902");
        let config = Config::load().unwrap();
        env::remove_var("TRI_AI_WORKER_PORT");
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();
        assert_eq!(config.worker.port, 9902);
    }

    #[test]
    fn resolves_relative_paths_from_config_location_and_project_root() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = temp_config(
            r#"
                [paths]
                project_root = "workspace"
                models_dir = "weights"
                ollama_root = "ollama"
                lm_studio_roots = ["lm-a", "lm-b"]
                jan_roots = ["jan"]
                stage_dir = "cache/staging"
                llama_server = "vendor/llama-server"
                event_log = "logs/events.jsonl"
            "#,
        );
        env::set_var("TRI_AI_CONFIG", &path);
        let config = Config::load().unwrap();
        env::remove_var("TRI_AI_CONFIG");
        let base = path.parent().unwrap().join("workspace");
        fs::remove_file(path).unwrap();

        assert_eq!(config.paths.project_root, base);
        assert_eq!(config.models_dir(), base.join("weights"));
        assert_eq!(config.paths.ollama_root, Some(base.join("ollama")));
        assert_eq!(
            config.paths.lm_studio_roots,
            vec![base.join("lm-a"), base.join("lm-b")]
        );
        assert_eq!(config.paths.jan_roots, vec![base.join("jan")]);
        assert_eq!(config.paths.stage_dir, base.join("cache/staging"));
        assert_eq!(config.paths.llama_server, base.join("vendor/llama-server"));
        assert_eq!(config.paths.event_log, base.join("logs/events.jsonl"));
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = env::temp_dir().join(format!(
            "missing-tri-ai-config-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        env::set_var("TRI_AI_CONFIG", &path);
        let result = Config::load();
        env::remove_var("TRI_AI_CONFIG");
        assert!(matches!(result, Err(ConfigError::Io { .. })));
    }

    #[test]
    fn external_bind_requires_authentication() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = temp_config("[server]\nlisten_addr = \"0.0.0.0:8900\"\n");
        env::set_var("TRI_AI_CONFIG", &path);
        let result = Config::load();
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();
        assert!(
            matches!(result, Err(ConfigError::InvalidValue(message)) if message.contains("AUTH_TOKEN"))
        );
    }

    #[test]
    fn external_bind_accepts_configured_authentication() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path =
            temp_config("[server]\nlisten_addr = \"0.0.0.0:8900\"\nauth_token = \"test-token\"\n");
        env::set_var("TRI_AI_CONFIG", &path);
        let config = Config::load().unwrap();
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();
        assert_eq!(config.server.auth_token.as_deref(), Some("test-token"));
    }

    #[test]
    fn server_user_id_is_server_configured_and_validated() {
        let _guard = ENV_GUARD.lock().unwrap();
        let path = temp_config("[server]\nuser_id = \"team_42\"\n");
        env::set_var("TRI_AI_CONFIG", &path);
        assert_eq!(Config::load().unwrap().server.user_id, "team_42");
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();

        let path = temp_config("[server]\nuser_id = \"not allowed\"\n");
        env::set_var("TRI_AI_CONFIG", &path);
        assert!(matches!(Config::load(), Err(ConfigError::InvalidValue(_))));
        env::remove_var("TRI_AI_CONFIG");
        fs::remove_file(path).unwrap();
    }
}
