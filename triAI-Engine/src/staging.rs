use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub block_id: String,
    pub generation: u64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct StageStore {
    root: PathBuf,
}

impl StageStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn stage_bytes(
        &self,
        block_id: &str,
        generation: u64,
        data: &[u8],
    ) -> io::Result<CommitRecord> {
        validate_id(block_id)?;
        let part = self.part_path(block_id, generation);
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        self.commit_part(block_id, generation, None)
    }

    /// Öffnet (oder setzt fort) die `.part`-Datei eines Blocks für
    /// inkrementelles Schreiben, z.B. per Download-Chunks. Anders als
    /// [`stage_bytes`](Self::stage_bytes) hält das den Inhalt nie komplett im
    /// Speicher — geeignet für mehrere Gigabyte große Downloads. Gibt die
    /// bereits vorhandene Größe zurück, damit ein Aufrufer einen
    /// abgebrochenen Download per HTTP-Range-Header fortsetzen kann, statt
    /// bei Null neu anzufangen.
    pub fn open_part_for_resume(&self, block_id: &str, generation: u64) -> io::Result<(File, u64)> {
        validate_id(block_id)?;
        let part = self.part_path(block_id, generation);
        let file = OpenOptions::new().create(true).append(true).open(&part)?;
        let len = file.metadata()?.len();
        Ok((file, len))
    }

    /// Schließt einen per [`open_part_for_resume`](Self::open_part_for_resume)
    /// geschriebenen Block ab: optionale SHA256-Prüfung gegen die bereits
    /// vollständig geschriebene `.part`-Datei, danach derselbe atomare
    /// rename+fsync-Pfad wie `stage_bytes`. Bei einem SHA256-Mismatch bleibt
    /// kein Commit zurück — die `.part`-Datei wird gelöscht.
    pub fn finalize_part(
        &self,
        block_id: &str,
        generation: u64,
        expected_sha256: Option<&str>,
    ) -> io::Result<CommitRecord> {
        validate_id(block_id)?;
        // Erzwingt, dass alle Schreiber der `.part`-Datei (der offene Handle
        // aus `open_part_for_resume` kann in einem anderen Thread liegen)
        // vor dem Hashen durchsynct sind.
        OpenOptions::new()
            .append(true)
            .open(self.part_path(block_id, generation))?
            .sync_all()?;
        self.commit_part(block_id, generation, expected_sha256)
    }

    /// Verwirft eine noch offene `.part`-Datei, z.B. nach Abbruch oder einer
    /// fehlgeschlagenen Validierung. Eine bereits fehlende Datei ist kein
    /// Fehler.
    pub fn discard_part(&self, block_id: &str, generation: u64) -> io::Result<()> {
        validate_id(block_id)?;
        match fs::remove_file(self.part_path(block_id, generation)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn part_path(&self, block_id: &str, generation: u64) -> PathBuf {
        self.root.join(format!("{block_id}.{generation}.part"))
    }

    /// Gemeinsamer Abschluss für `stage_bytes` und `finalize_part`: hasht die
    /// bereits vollständig geschriebene `.part`-Datei, prüft sie optional
    /// gegen `expected_sha256`, und benennt sie andernfalls atomar in `.bin`
    /// um, gefolgt vom `.commit`-Marker und einem Directory-fsync.
    fn commit_part(
        &self,
        block_id: &str,
        generation: u64,
        expected_sha256: Option<&str>,
    ) -> io::Result<CommitRecord> {
        let part = self.part_path(block_id, generation);
        let blob = self.root.join(format!("{block_id}.{generation}.bin"));
        let commit = self.root.join(format!("{block_id}.{generation}.commit"));
        let bytes = fs::metadata(&part)?.len();
        let sha = sha256(&part)?;
        if let Some(expected) = expected_sha256 {
            if !sha.eq_ignore_ascii_case(expected) {
                let _ = fs::remove_file(&part);
                crate::observability::emit(
                    "staging_commit_rejected",
                    serde_json::json!({
                        "block_id": block_id,
                        "generation": generation,
                        "reason": "sha256_mismatch",
                    }),
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SHA256-Mismatch: erwartet {expected}, erhalten {sha}"),
                ));
            }
        }
        let record = CommitRecord {
            block_id: block_id.into(),
            generation,
            bytes,
            sha256: sha,
        };
        fs::rename(&part, &blob)?;
        let mut c = File::create(&commit)?;
        c.write_all(
            format!(
                "{}\n{}\n{}\n{}\n",
                record.block_id, record.generation, record.bytes, record.sha256
            )
            .as_bytes(),
        )?;
        c.sync_all()?;
        // `sync_all()` on the files themselves durably persists their
        // *content*, but on ext4/xfs a `rename()`/new file's *directory
        // entry* is a separate write that the filesystem is free to keep
        // unpersisted until the directory itself is fsynced. Without this,
        // a crash right after this point could — on some filesystems —
        // lose the rename of `.bin` or the creation of `.commit` even
        // though their contents made it to disk, defeating the whole
        // point of a durable commit marker.
        fsync_dir(&self.root)?;
        crate::observability::emit(
            "staging_committed",
            serde_json::json!({
                "block_id": record.block_id,
                "generation": record.generation,
                "bytes": record.bytes,
                "sha256": record.sha256,
            }),
        );
        Ok(record)
    }

    pub fn recover(&self) -> io::Result<Vec<CommitRecord>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("commit") {
                continue;
            }
            let lines: Vec<_> = fs::read_to_string(&path)?
                .lines()
                .map(str::to_owned)
                .collect();
            if lines.len() != 4 {
                continue;
            }
            let rec = CommitRecord {
                block_id: lines[0].clone(),
                generation: lines[1].parse().unwrap_or(0),
                bytes: lines[2].parse().unwrap_or(0),
                sha256: lines[3].clone(),
            };
            let blob = self
                .root
                .join(format!("{}.{}.bin", rec.block_id, rec.generation));
            if blob.is_file()
                && fs::metadata(&blob)?.len() == rec.bytes
                && sha256(&blob).ok().as_deref() == Some(rec.sha256.as_str())
            {
                out.push(rec);
            }
        }
        out.sort_by(|a, b| {
            a.block_id
                .cmp(&b.block_id)
                .then(a.generation.cmp(&b.generation))
        });
        Ok(out)
    }

    /// Restore a verified commit's bytes onto `target`, outside the staging root.
    ///
    /// Looks up `block_id`+`generation` via `recover()`, which only yields records
    /// whose `.bin` size and sha256 still match their `.commit` metadata. If no such
    /// record exists (missing, size mismatch, checksum mismatch), returns `Err` and
    /// never touches `target` or its directory in any way — an existing `target`
    /// stays byte-for-byte as it was. Only once the checksum is confirmed does this
    /// write a temp file next to `target` and `rename` it into place atomically.
    pub fn restore_active(&self, block_id: &str, generation: u64, target: &Path) -> io::Result<()> {
        validate_id(block_id)?;
        let record = match self
            .recover()?
            .into_iter()
            .find(|r| r.block_id == block_id && r.generation == generation)
        {
            Some(record) => record,
            None => {
                crate::observability::emit(
                    "staging_restore_failed",
                    serde_json::json!({
                        "block_id": block_id,
                        "generation": generation,
                        "reason": "no_verified_commit",
                    }),
                );
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no verified commit for block '{block_id}' generation {generation}"),
                ));
            }
        };

        let blob = self
            .root
            .join(format!("{}.{}.bin", record.block_id, record.generation));

        let target_dir = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "restore target has no parent directory",
            )
        })?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = target_dir.join(format!(
            ".restore-{}-{}-{}-{nanos}.tmp",
            record.block_id,
            record.generation,
            std::process::id()
        ));
        let restore_failed = |err: &io::Error| {
            crate::observability::emit(
                "staging_restore_failed",
                serde_json::json!({
                    "block_id": block_id,
                    "generation": generation,
                    "reason": "io_error",
                    "error_kind": format!("{:?}", err.kind()),
                }),
            );
        };
        if let Err(e) = fs::copy(&blob, &tmp) {
            let _ = fs::remove_file(&tmp);
            restore_failed(&e);
            return Err(e);
        }
        let sync_result = OpenOptions::new()
            .write(true)
            .open(&tmp)
            .and_then(|f| f.sync_all());
        if let Err(e) = sync_result {
            let _ = fs::remove_file(&tmp);
            restore_failed(&e);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp, target) {
            let _ = fs::remove_file(&tmp);
            restore_failed(&e);
            return Err(e);
        }
        // Same directory-durability gap as in `stage_bytes`: fsync the
        // directory the rename landed in, not just the file's own content.
        if let Err(e) = fsync_dir(target_dir) {
            restore_failed(&e);
            return Err(e);
        }
        crate::observability::emit(
            "staging_restored",
            serde_json::json!({
                "block_id": record.block_id,
                "generation": record.generation,
                "bytes": record.bytes,
            }),
        );
        Ok(())
    }

    pub fn cleanup_incomplete(&self) -> io::Result<usize> {
        let mut n = 0;
        for entry in fs::read_dir(&self.root)? {
            let p = entry?.path();
            if p.extension().and_then(|x| x.to_str()) == Some("part") {
                fs::remove_file(p)?;
                n += 1;
            }
        }
        Ok(n)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// `File::open` on a directory is valid on Unix purely for `fsync`ing it —
/// this durably persists directory-entry changes (renames, new files) in
/// that directory, which a plain `File::sync_all()` on the *entry itself*
/// does not cover. Review-Finding 3.
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid block id",
        ));
    }
    Ok(())
}

fn sha256(path: &Path) -> io::Result<String> {
    let out = Command::new("sha256sum").arg(path).output()?;
    if !out.status.success() {
        return Err(io::Error::other("sha256sum failed"));
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("sha256sum returned no digest"))
}

#[cfg(test)]
fn unique_test_dir() -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("tri-ai-staging-{}-{now}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_and_recover() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        let r = s.stage_bytes("block0", 1, b"abc").unwrap();
        assert_eq!(r.bytes, 3);
        assert_eq!(s.recover().unwrap(), vec![r]);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn incomplete_is_cleaned() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        fs::write(d.join("bad.1.part"), b"x").unwrap();
        assert_eq!(s.cleanup_incomplete().unwrap(), 1);
        assert!(s.recover().unwrap().is_empty());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn traversal_is_rejected() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        assert!(s.stage_bytes("../escape", 1, b"x").is_err());
        assert!(s
            .restore_active("../escape", 1, &d.join("target.bin"))
            .is_err());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn restore_active_writes_staged_bytes() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        s.stage_bytes("block0", 1, b"hello world").unwrap();
        let target = d.join("target.bin");
        s.restore_active("block0", 1, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello world");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn restore_active_rejects_checksum_mismatch_and_leaves_target_untouched() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        let r = s.stage_bytes("block0", 1, b"original bytes").unwrap();
        // Tamper with the committed blob after the fact so its sha256 no longer
        // matches the recorded checksum.
        let blob = d.join(format!("{}.{}.bin", r.block_id, r.generation));
        fs::write(&blob, b"corrupted!!!").unwrap();

        let target = d.join("target.bin");
        fs::write(&target, b"pre-existing content").unwrap();

        let err = s.restore_active("block0", 1, &target).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read(&target).unwrap(), b"pre-existing content");
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn restore_active_unknown_record_errors_without_side_effects() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        let target = d.join("target.bin");
        let err = s.restore_active("no-such-block", 1, &target).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!target.exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn open_part_for_resume_reports_zero_for_fresh_block() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        let (_file, len) = s.open_part_for_resume("dl-block", 1).unwrap();
        assert_eq!(len, 0);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn open_part_for_resume_reports_existing_bytes_after_reopen() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        {
            let (mut file, len) = s.open_part_for_resume("dl-block", 1).unwrap();
            assert_eq!(len, 0);
            file.write_all(b"partial-chunk").unwrap();
            file.sync_all().unwrap();
        }
        let (_file, len) = s.open_part_for_resume("dl-block", 1).unwrap();
        assert_eq!(len, "partial-chunk".len() as u64);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn finalize_part_commits_streamed_bytes_and_is_recoverable() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        {
            let (mut file, _) = s.open_part_for_resume("dl-block", 1).unwrap();
            file.write_all(b"hello ").unwrap();
            file.write_all(b"world").unwrap();
        }
        let record = s.finalize_part("dl-block", 1, None).unwrap();
        assert_eq!(record.bytes, 11);
        assert_eq!(s.recover().unwrap(), vec![record]);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn finalize_part_verifies_matching_sha256() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        {
            let (mut file, _) = s.open_part_for_resume("dl-block", 1).unwrap();
            file.write_all(b"abc").unwrap();
        }
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let record = s.finalize_part("dl-block", 1, Some(expected)).unwrap();
        assert_eq!(record.sha256, expected);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn finalize_part_rejects_sha256_mismatch_and_removes_part() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        {
            let (mut file, _) = s.open_part_for_resume("dl-block", 1).unwrap();
            file.write_all(b"corrupted download").unwrap();
        }
        let wrong = "0".repeat(64);
        let err = s.finalize_part("dl-block", 1, Some(&wrong)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(s.recover().unwrap().is_empty());
        assert!(!d.join("dl-block.1.part").exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn discard_part_removes_in_progress_download_without_error_if_absent() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        s.discard_part("no-such-download", 1).unwrap();
        {
            let (mut file, _) = s.open_part_for_resume("dl-block", 1).unwrap();
            file.write_all(b"abandoned").unwrap();
        }
        s.discard_part("dl-block", 1).unwrap();
        assert!(!d.join("dl-block.1.part").exists());
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn open_part_for_resume_rejects_traversal_in_block_id() {
        let d = unique_test_dir();
        let s = StageStore::new(&d).unwrap();
        assert!(s.open_part_for_resume("../escape", 1).is_err());
        fs::remove_dir_all(d).unwrap();
    }
}
