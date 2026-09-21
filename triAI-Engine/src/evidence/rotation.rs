//! 14-Tage Log-Rotation für Evidence-Dateien.
//! Crash-safe: Atomic deletes via rename-before-unlink.
//! Deterministisch testbar mit Mock-Clock.

use crate::error::{Result, TriAIError};
use chrono::{DateTime, Duration, Utc};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::time::{interval_at, Instant};

/// Konfiguration für die Rotation
#[derive(Debug, Clone)]
pub struct RotationConfig {
    pub base_path: PathBuf,
    pub retention_days: u32,
    pub dry_run: bool,
}

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("data/evidence"),
            retention_days: 14,
            dry_run: false,
        }
    }
}

/// Ergebnis einer Rotation
#[derive(Debug, Clone, serde::Serialize)]
pub struct RotationReport {
    pub timestamp: DateTime<Utc>,
    pub files_scanned: u32,
    pub files_deleted: u32,
    pub bytes_freed: u64,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

/// Information über eine zu rotierende Datei
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: DateTime<Utc>,
    pub age_days: u32,
}

/// Clock-Trait für deterministisches Testen
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// System-Clock (Produktion)
#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Mock-Clock für Tests
#[derive(Debug, Clone)]
pub struct MockClock {
    pub fixed_time: DateTime<Utc>,
}

impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        self.fixed_time
    }
}

/// Rotations-Manager
pub struct RotationManager<C: Clock = SystemClock> {
    config: RotationConfig,
    clock: C,
}

impl RotationManager<SystemClock> {
    /// Erstellt einen neuen Manager mit System-Clock
    pub fn new(config: RotationConfig) -> Self {
        Self {
            config,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> RotationManager<C> {
    /// Erstellt einen Manager mit custom Clock (für Tests)
    pub fn with_clock(config: RotationConfig, clock: C) -> Self {
        Self { config, clock }
    }

    /// Listet alle abgelaufenen Dateien auf (ohne zu löschen)
    pub async fn list_expired(&self) -> Result<Vec<FileInfo>> {
        let cutoff = self.clock.now() - Duration::days(self.config.retention_days as i64);
        let mut expired = Vec::new();

        let mut entries = fs::read_dir(&self.config.base_path).await.map_err(|e| {
            TriAIError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to read evidence dir: {}", e),
            ))
        })?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // Nur .jsonl Dateien betrachten
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }

            let metadata = entry.metadata().await?;
            let modified = metadata
                .modified()
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| self.clock.now());

            if modified < cutoff {
                let age_days = (self.clock.now() - modified).num_days() as u32;
                expired.push(FileInfo {
                    path,
                    size_bytes: metadata.len(),
                    modified,
                    age_days,
                });
            }
        }

        // Sortiere nach Alter (älteste zuerst)
        expired.sort_by(|a, b| a.modified.cmp(&b.modified));
        Ok(expired)
    }

    /// Führt die Rotation durch (löscht abgelaufene Dateien)
    pub async fn rotate(&self) -> Result<RotationReport> {
        let expired = self.list_expired().await?;
        let mut report = RotationReport {
            timestamp: self.clock.now(),
            files_scanned: expired.len() as u32,
            files_deleted: 0,
            bytes_freed: 0,
            errors: Vec::new(),
            dry_run: self.config.dry_run,
        };

        for file in expired {
            if self.config.dry_run {
                tracing::info!(
                    path = %file.path.display(),
                    age_days = file.age_days,
                    size_bytes = file.size_bytes,
                    "[DRY-RUN] Would rotate file"
                );
                report.files_deleted += 1;
                report.bytes_freed += file.size_bytes;
                continue;
            }

            // Crash-safe delete: rename to .deleting, then unlink
            let deleting_path = file.path.with_extension("jsonl.deleting");

            match fs::rename(&file.path, &deleting_path).await {
                Ok(_) => match fs::remove_file(&deleting_path).await {
                    Ok(_) => {
                        tracing::info!(
                            path = %file.path.display(),
                            age_days = file.age_days,
                            size_bytes = file.size_bytes,
                            "Rotated evidence file"
                        );
                        report.files_deleted += 1;
                        report.bytes_freed += file.size_bytes;
                    }
                    Err(e) => {
                        let msg = format!("Failed to unlink {}: {}", deleting_path.display(), e);
                        tracing::error!("{}", msg);
                        report.errors.push(msg);
                    }
                },
                Err(e) => {
                    let msg = format!(
                        "Failed to rename {} for deletion: {}",
                        file.path.display(),
                        e
                    );
                    tracing::error!("{}", msg);
                    report.errors.push(msg);
                }
            }
        }

        tracing::info!(
            files_deleted = report.files_deleted,
            bytes_freed = report.bytes_freed,
            dry_run = report.dry_run,
            "Rotation complete"
        );

        Ok(report)
    }

    /// Startet periodische Rotation im Hintergrund
    pub async fn run_periodic(self, interval_hours: u64) {
        let interval_duration = std::time::Duration::from_secs(interval_hours * 3600);
        let start = Instant::now() + interval_duration;
        let mut interval = interval_at(start, interval_duration);

        tracing::info!(
            interval_hours = interval_hours,
            retention_days = self.config.retention_days,
            "Starting periodic rotation task"
        );

        loop {
            interval.tick().await;

            match self.rotate().await {
                Ok(report) => {
                    tracing::info!(
                        files_deleted = report.files_deleted,
                        bytes_freed = report.bytes_freed,
                        "Periodic rotation completed"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Periodic rotation failed");
                }
            }
        }
    }
}

/// Bereinigt orphaned `.deleting` Dateien (von abgestürzten Rotationen)
pub async fn cleanup_orphaned_deleting(base_path: &Path) -> Result<u32> {
    let mut cleaned = 0;
    let mut entries = fs::read_dir(base_path).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("deleting") {
            if let Err(e) = fs::remove_file(&path).await {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to cleanup orphaned .deleting file"
                );
            } else {
                tracing::info!(
                    path = %path.display(),
                    "Cleaned up orphaned .deleting file"
                );
                cleaned += 1;
            }
        }
    }

    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_list_expired_files() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        let old_file = base_path.join("old.jsonl");
        let new_file = base_path.join("new.jsonl");

        fs::write(&old_file, "old data\n").await.unwrap();
        fs::write(&new_file, "new data\n").await.unwrap();

        // Setze modified time für old_file auf 20 Tage zurück
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(20 * 86400);
        let file = std::fs::File::open(&old_file).unwrap();
        file.set_modified(old_time).unwrap();

        let clock = MockClock {
            fixed_time: Utc::now(),
        };

        let config = RotationConfig {
            base_path: base_path.clone(),
            retention_days: 14,
            dry_run: false,
        };

        let manager = RotationManager::with_clock(config, clock);
        let expired = manager.list_expired().await.unwrap();

        assert_eq!(expired.len(), 1);
        assert!(expired[0].path.ends_with("old.jsonl"));
        assert!(expired[0].age_days >= 20);
    }

    #[tokio::test]
    async fn test_rotate_deletes_old_files() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        let old_file = base_path.join("old.jsonl");
        fs::write(&old_file, "old data\n").await.unwrap();

        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(20 * 86400);
        let file = std::fs::File::open(&old_file).unwrap();
        file.set_modified(old_time).unwrap();

        let clock = MockClock {
            fixed_time: Utc::now(),
        };

        let config = RotationConfig {
            base_path: base_path.clone(),
            retention_days: 14,
            dry_run: false,
        };

        let manager = RotationManager::with_clock(config, clock);
        let report = manager.rotate().await.unwrap();

        assert_eq!(report.files_deleted, 1);
        assert!(!old_file.exists());
        assert!(!base_path.join("old.jsonl.deleting").exists());
    }

    #[tokio::test]
    async fn test_rotate_dry_run_does_not_delete() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        let old_file = base_path.join("old.jsonl");
        fs::write(&old_file, "old data\n").await.unwrap();

        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(20 * 86400);
        let file = std::fs::File::open(&old_file).unwrap();
        file.set_modified(old_time).unwrap();

        let clock = MockClock {
            fixed_time: Utc::now(),
        };

        let config = RotationConfig {
            base_path: base_path.clone(),
            retention_days: 14,
            dry_run: true,
        };

        let manager = RotationManager::with_clock(config, clock);
        let report = manager.rotate().await.unwrap();

        assert_eq!(report.files_deleted, 1); // Würde gelöscht
        assert!(report.dry_run);
        assert!(old_file.exists()); // Aber existiert noch!
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_deleting() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        let orphaned = base_path.join("orphaned.jsonl.deleting");
        fs::write(&orphaned, "orphaned data\n").await.unwrap();

        let cleaned = cleanup_orphaned_deleting(&base_path).await.unwrap();

        assert_eq!(cleaned, 1);
        assert!(!orphaned.exists());
    }

    #[tokio::test]
    async fn test_rotation_preserves_new_files() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        let new_file = base_path.join("new.jsonl");
        fs::write(&new_file, "new data\n").await.unwrap();

        let clock = MockClock {
            fixed_time: Utc::now(),
        };

        let config = RotationConfig {
            base_path: base_path.clone(),
            retention_days: 14,
            dry_run: false,
        };

        let manager = RotationManager::with_clock(config, clock);
        let report = manager.rotate().await.unwrap();

        assert_eq!(report.files_deleted, 0);
        assert!(new_file.exists());
    }
}
