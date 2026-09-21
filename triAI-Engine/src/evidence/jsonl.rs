//! Async, buffered JSON-Lines writer with secret redaction.
//! Non-blocking: inference path never waits for disk I/O.

use crate::error::{Result, TriAIError};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;

/// Patterns that indicate secrets (case-insensitive)
const SECRET_PATTERNS: &[&str] = &[
    "sk-",
    "api_key",
    "password",
    "secret",
    "token",
    "bearer",
    "authorization",
    "x-api-key",
    "openai-",
    "anthropic-",
    "claude-",
    "gpt-",
    "aws_access",
    "aws_secret",
];

/// Configuration for the JSONL writer
#[derive(Debug, Clone)]
pub struct JsonlConfig {
    pub base_path: PathBuf,
    pub flush_interval_ms: u64,
    pub buffer_size_bytes: usize,
    pub rotation_days: u32,
}

impl Default for JsonlConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("data/evidence"),
            flush_interval_ms: 100,
            buffer_size_bytes: 64 * 1024, // 64 KB
            rotation_days: 14,
        }
    }
}

/// Message sent to the background writer task
enum WriterMessage {
    Write {
        filename: String,
        data: Vec<u8>,
    },
    Flush {
        response: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        response: oneshot::Sender<Result<()>>,
    },
}

/// Async JSON-Lines writer with buffering and secret redaction
pub struct JsonlWriter {
    sender: mpsc::Sender<WriterMessage>,
    config: JsonlConfig,
}

impl JsonlWriter {
    /// Create a new writer and start the background task
    pub async fn new(config: JsonlConfig) -> Result<Self> {
        // Ensure base directory exists
        tokio::fs::create_dir_all(&config.base_path).await?;

        let (sender, receiver) = mpsc::channel(1000);

        // Spawn background writer task
        let bg_config = config.clone();
        tokio::spawn(async move {
            if let Err(e) = writer_task(receiver, bg_config).await {
                tracing::error!("Evidence writer task failed: {}", e);
            }
        });

        Ok(Self { sender, config })
    }

    /// Returns a reference to the writer configuration.
    pub fn config(&self) -> &JsonlConfig {
        &self.config
    }

    /// Write a serializable record to the specified file (non-blocking)
    pub async fn write<T: Serialize>(&self, filename: &str, record: &T) -> Result<()> {
        let json = serde_json::to_string(record)?;

        // Redact secrets before writing
        let redacted = redact_secrets(&json);

        let mut data = redacted.into_bytes();
        data.push(b'\n');

        self.sender
            .send(WriterMessage::Write {
                filename: filename.to_string(),
                data,
            })
            .await
            .map_err(|_| TriAIError::EvidenceWrite("Writer channel closed".into()))?;

        Ok(())
    }

    /// Flush all buffers to disk (blocking)
    pub async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(WriterMessage::Flush { response: tx })
            .await
            .map_err(|_| TriAIError::EvidenceWrite("Writer channel closed".into()))?;
        rx.await
            .map_err(|_| TriAIError::EvidenceWrite("Flush response dropped".into()))?
    }

    /// Graceful shutdown: flush and close all files
    pub async fn shutdown(self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(WriterMessage::Shutdown { response: tx })
            .await
            .map_err(|_| TriAIError::EvidenceWrite("Writer channel closed".into()))?;
        rx.await
            .map_err(|_| TriAIError::EvidenceWrite("Shutdown response dropped".into()))?
    }
}

/// Background task that handles all disk I/O
async fn writer_task(
    mut receiver: mpsc::Receiver<WriterMessage>,
    config: JsonlConfig,
) -> Result<()> {
    let mut buffers: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut files: std::collections::HashMap<String, File> = std::collections::HashMap::new();
    let mut flush_interval = interval(Duration::from_millis(config.flush_interval_ms));

    loop {
        tokio::select! {
            // Handle incoming messages
            msg = receiver.recv() => {
                match msg {
                    Some(WriterMessage::Write { filename, data }) => {
                        let buffer = buffers.entry(filename.clone()).or_insert_with(Vec::new);
                        buffer.extend(data);

                        // Flush if buffer exceeds size threshold
                        if buffer.len() >= config.buffer_size_bytes {
                            if let Err(e) = flush_buffer(&filename, buffer, &mut files, &config.base_path).await {
                                tracing::error!("Failed to flush buffer for {}: {}", filename, e);
                            }
                        }
                    }
                    Some(WriterMessage::Flush { response }) => {
                        let result = flush_all_buffers(&mut buffers, &mut files, &config.base_path).await;
                        let _ = response.send(result);
                    }
                    Some(WriterMessage::Shutdown { response }) => {
                        // Flush all remaining data
                        let result = flush_all_buffers(&mut buffers, &mut files, &config.base_path).await;

                        // Sync all files to disk
                        for file in files.values_mut() {
                            if let Err(e) = file.sync_all().await {
                                tracing::error!("Failed to sync file: {}", e);
                            }
                        }

                        let _ = response.send(result);
                        break;
                    }
                    None => {
                        // Channel closed, flush and exit
                        let _ = flush_all_buffers(&mut buffers, &mut files, &config.base_path).await;
                        break;
                    }
                }
            }

            // Periodic flush
            _ = flush_interval.tick() => {
                if let Err(e) = flush_all_buffers(&mut buffers, &mut files, &config.base_path).await {
                    tracing::error!("Periodic flush failed: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Flush a single buffer to disk
async fn flush_buffer(
    filename: &str,
    buffer: &mut Vec<u8>,
    files: &mut std::collections::HashMap<String, File>,
    base_path: &Path,
) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }

    let file = if let Some(f) = files.get_mut(filename) {
        f
    } else {
        let path = base_path.join(filename);
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        files.entry(filename.to_string()).or_insert(f)
    };

    file.write_all(buffer).await?;
    buffer.clear();

    Ok(())
}

/// Flush all buffers to disk
async fn flush_all_buffers(
    buffers: &mut std::collections::HashMap<String, Vec<u8>>,
    files: &mut std::collections::HashMap<String, File>,
    base_path: &Path,
) -> Result<()> {
    for (filename, buffer) in buffers.iter_mut() {
        flush_buffer(filename, buffer, files, base_path).await?;
    }
    Ok(())
}

/// Redact potential secrets from JSON strings
fn redact_secrets(text: &str) -> String {
    let mut result = text.to_string();

    for pattern in SECRET_PATTERNS {
        // Case-insensitive search and replace
        if let Ok(regex) = regex::Regex::new(&format!(r"(?i){}[^,\}}\]]*", regex::escape(pattern)))
        {
            result = regex
                .replace_all(&result, &format!("{}[REDACTED]", pattern))
                .to_string();
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_and_read() {
        let temp_dir = TempDir::new().unwrap();
        let config = JsonlConfig {
            base_path: temp_dir.path().to_path_buf(),
            flush_interval_ms: 50,
            buffer_size_bytes: 1024,
            rotation_days: 14,
        };

        let writer = JsonlWriter::new(config).await.unwrap();

        let record = serde_json::json!({
            "run_id": "test-123",
            "model_id": "test-model"
        });

        writer.write("test.jsonl", &record).await.unwrap();
        writer.flush().await.unwrap();

        let path = temp_dir.path().join("test.jsonl");
        let content = tokio::fs::read_to_string(path).await.unwrap();
        assert!(content.contains("test-123"));
    }

    #[test]
    fn test_secret_redaction() {
        let input = r#"{"api_key": "sk-12345", "prompt": "test"}"#;
        let redacted = redact_secrets(input);
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains("sk-12345"));
    }

    #[tokio::test]
    async fn test_buffer_flush_on_size() {
        let temp_dir = TempDir::new().unwrap();
        let config = JsonlConfig {
            base_path: temp_dir.path().to_path_buf(),
            flush_interval_ms: 10000, // Long interval
            buffer_size_bytes: 100,   // Small buffer
            rotation_days: 14,
        };

        let writer = JsonlWriter::new(config).await.unwrap();

        // Write enough data to trigger buffer flush
        for i in 0..10 {
            let record = serde_json::json!({"id": i, "data": "x".repeat(20)});
            writer.write("test.jsonl", &record).await.unwrap();
        }

        // Wait a bit for background task
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = temp_dir.path().join("test.jsonl");
        assert!(path.exists());
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Optional: JsonlWriter + RotationManager combined
// ──────────────────────────────────────────────────────────────────────────────

/// Combined configuration for writer + rotation
#[derive(Debug, Clone)]
pub struct JsonlConfigWithRotation {
    pub writer: JsonlConfig,
    pub retention_days: u32,
    pub rotation_interval_hours: u64,
    pub dry_run: bool,
}

impl Default for JsonlConfigWithRotation {
    fn default() -> Self {
        Self {
            writer: JsonlConfig::default(),
            retention_days: 14,
            rotation_interval_hours: 24,
            dry_run: false,
        }
    }
}

/// JsonlWriter with an optional background RotationManager
pub struct JsonlWriterWithRotation {
    pub writer: JsonlWriter,
}

impl JsonlWriterWithRotation {
    /// Creates a new writer and spawns a RotationManager in the background.
    pub async fn new(config: JsonlConfigWithRotation) -> Result<Self> {
        use crate::evidence::rotation::{RotationConfig, RotationManager};

        let rotation_config = RotationConfig {
            base_path: config.writer.base_path.clone(),
            retention_days: config.retention_days,
            dry_run: config.dry_run,
        };

        let interval_hours = config.rotation_interval_hours;
        let writer = JsonlWriter::new(config.writer).await?;

        tokio::spawn(async move {
            RotationManager::new(rotation_config)
                .run_periodic(interval_hours)
                .await;
        });

        Ok(Self { writer })
    }

    /// Delegates to the inner JsonlWriter
    pub async fn write<T: serde::Serialize>(&self, filename: &str, record: &T) -> Result<()> {
        self.writer.write(filename, record).await
    }

    /// Graceful shutdown
    pub async fn shutdown(self) -> Result<()> {
        self.writer.shutdown().await
    }
}
