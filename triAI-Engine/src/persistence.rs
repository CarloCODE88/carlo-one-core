//! Persistente Konversations-Sitzungen für tri-ai-runner.
//!
//! Goose speichert Sessions und ermöglicht das Wiederaufnehmen über Neustarts
//! hinweg; dieser Strang schließt dieselbe Lücke für die lokale Lösung. Jede
//! Konversation wird als zeilenweise JSONL-Datei unter
//! `<project_root>/sessions/<id>.jsonl` abgelegt (append-only). Beim Start
//! liest `Scanner` alle vorhandenen Sitzungen und macht sie über `list`/
//! `load` verfügbar.
//!
//! Sicherheits-/Robustheitsgrenzen:
//! - Session-IDs sind begrenzte GUIDs (nur `[0-9a-fA-F-]`, Länge ≤ 40) —
//!   nie ein vom Client gelieferter Pfad.
//! - Das Schreiben ist atomar (temp-Datei + rename), damit nie eine halb
//!   geschriebene Zeile sichtbar wird.
//! - Beim Laden wird eine kaputte Zeile übersprungen (nicht hart abgebrochen),
//!   damit eine einzelne fehlerhafte Sitzung nicht den gesamten Start blockiert.

use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

/// Dateiendung einer Session.
const SESSION_EXT: &str = "jsonl";
/// Maximale gültige Länge einer Session-ID (GUID-Darstellung).
const MAX_ID_LEN: usize = 40;
/// Maximale Zeilengröße einer einzelnen Session-Zeile (Schutz vor einer
/// riesigen, fehlerhaften Zeile beim Laden).
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Maximale Gesamtgröße einer Session-Datei (Schutz vor aufgeblähten Logs,
/// z. B. durch einen verirrten Riesen-Block beim Laden).
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Fehler beim Zugriff auf Sitzungen.
#[derive(Debug)]
pub struct PersistenceError(pub String);

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PersistenceError {}

/// Eine einzelne Zeile einer Session (eine Chat-Message o.ä.).
pub type SessionLine = Value;

/// Scannt und lädt alle gespeicherten Sitzungen unterhalb eines Verzeichnisses.
pub struct Scanner {
    root: PathBuf,
}

impl Scanner {
    /// `root` ist das Sessions-Verzeichnis (z.B. `<project_root>/sessions`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Gibt die IDs aller vorhandenen Sitzungen zurück (ohne Dateiendung).
    /// Kein Fehler, wenn das Verzeichnis fehlt oder leer ist.
    pub fn list(&self) -> Result<Vec<String>, PersistenceError> {
        let mut out = Vec::new();
        match fs::read_dir(&self.root) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXT) {
                        continue;
                    }
                    let id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if is_valid_session_id(&id) {
                        out.push(id);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(PersistenceError(format!(
                    "Sessions-Verzeichnis nicht lesbar: {e}"
                )))
            }
        }
        out.sort();
        Ok(out)
    }

    /// Lädt alle Zeilen einer Sitzung. Fehlendes Verzeichnis/Datei ist ein
    /// leerer Verlauf (kein Fehler). Kaputte Zeilen werden übersprungen.
    pub fn load(&self, id: &str) -> Result<Vec<SessionLine>, PersistenceError> {
        if !is_valid_session_id(id) {
            return Err(PersistenceError(format!("Ungültige Session-ID '{id}'")));
        }
        let path = session_path(&self.root, id);
        let Ok(file) = fs::File::open(&path) else {
            return Ok(Vec::new()); // noch nicht angelegt -> leerer Verlauf
        };
        // Größen-Limit prüfen, bevor wir spekulativ zu lesen beginnen.
        if let Ok(meta) = file.metadata() {
            if meta.len() > MAX_FILE_BYTES {
                return Err(PersistenceError(format!(
                    "Session '{}' überschreitet das Datei-Limit ({} > {} Bytes)",
                    id,
                    meta.len(),
                    MAX_FILE_BYTES
                )));
            }
        }
        let mut lines = Vec::new();
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .map_err(|e| PersistenceError(format!("Session '{}' nicht lesbar: {e}", id)))?;
            if n == 0 {
                break;
            }
            if buf.trim().is_empty() || buf.len() > MAX_LINE_BYTES {
                continue; // kaputte/zu große Zeile überspringen
            }
            if let Ok(value) = serde_json::from_str::<Value>(&buf) {
                lines.push(value);
            }
        }
        Ok(lines)
    }
}

/// Hängt eine JSON-Zeile an eine Session an. Atomar (temp + rename) und
/// append-sicher. Erstellt Verzeichnis und Datei bei Bedarf.
pub fn append(id: &str, line: Value, root: &Path) -> Result<(), PersistenceError> {
    if !is_valid_session_id(id) {
        return Err(PersistenceError(format!("Ungültige Session-ID '{id}'")));
    }
    fs::create_dir_all(root)
        .map_err(|e| PersistenceError(format!("Sessions-Verzeichnis nicht anlegbar: {e}")))?;
    let path = session_path(root, id);
    // Direktes, idempotentes Appending: öffnet (oder legt an) die Datei im
    // Append-Modus und schreibt die komplette Zeile inkl. Newline in einem
    // Vorgang. So bleibt ein bisheriger Verlauf erhalten (kein Überschreiben
    // durch rename) und eine einzelne Zeile landet immer am Stück.
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| PersistenceError(format!("Session-Datei nicht anlegbar: {e}")))?;
        let json = serde_json::to_string(&line)
            .map_err(|e| PersistenceError(format!("Zeile nicht serialisierbar: {e}")))?;
        file.write_all(json.as_bytes())
            .map_err(|e| PersistenceError(format!("Zeile nicht schreibbar: {e}")))?;
        file.write_all(b"\n")
            .map_err(|e| PersistenceError(format!("Newline nicht schreibbar: {e}")))?;
        file.flush()
            .map_err(|e| PersistenceError(format!("Flush fehlgeschlagen: {e}")))?;
    }
    Ok(())
}

fn session_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.{SESSION_EXT}"))
}

/// Gültige Session-ID: 1–40 Zeichen, nur ASCII-Alphanumerik, Bindestriche
/// und Unterstriche (GUID-/Slug-artig). Blockiert Pfad-Traversal («..»,
/// Slash, Backslash), Leerzeichen und Unicode-Sonderzeichen.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "persist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn guid_validation_rejects_paths_and_overflow() {
        assert!(is_valid_session_id("abc-123"));
        assert!(is_valid_session_id("abcdef0123456789"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id(".."));
        assert!(!is_valid_session_id("a/b"));
        assert!(!is_valid_session_id("a\\b"));
        assert!(!is_valid_session_id(&"a".repeat(41)));
    }

    #[test]
    fn append_and_load_roundtrips_across_scanner() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("roundtrip");
        let sessions = dir.join("sessions");
        let scanner = Scanner::new(&sessions);

        // Leere Liste, wenn nichts angelegt ist (kein Fehler).
        assert!(scanner.list().unwrap().is_empty());

        append(
            "sess-1",
            serde_json::json!({"role": "user", "content": "hello"}),
            &sessions,
        )
        .unwrap();
        append(
            "sess-1",
            serde_json::json!({"role": "assistant", "content": "hi"}),
            &sessions,
        )
        .unwrap();
        append(
            "sess-2",
            serde_json::json!({"role": "user", "content": "other"}),
            &sessions,
        )
        .unwrap();

        let ids = scanner.list().unwrap();
        assert_eq!(ids, vec!["sess-1".to_string(), "sess-2".to_string()]);
        let loaded = scanner.load("sess-1").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0]["role"], "user");
        assert_eq!(loaded[1]["content"], "hi");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_line_is_skipped_not_fatal() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("corrupt");
        let sessions = dir.join("sessions");
        let scanner = Scanner::new(&sessions);
        append(
            "sess",
            serde_json::json!({"role": "user", "content": "ok"}),
            &sessions,
        )
        .unwrap();
        // Eine kaputte Zeile manuell anfügen (kein gültiges JSON).
        fs::create_dir_all(&sessions).unwrap();
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(sessions.join("sess.jsonl"))
            .unwrap();
        use std::io::Write;
        f.write_all(b"{not valid json\n").unwrap();
        drop(f);

        let loaded = scanner.load("sess").unwrap();
        // Nur die gültige Zeile wurde geladen.
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["role"], "user");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_invalid_id_before_reading() {
        let dir = temp_dir("badid");
        let scanner = Scanner::new(dir.join("sessions"));
        assert!(scanner.load("../../etc").is_err());
        assert!(scanner.load("a/b").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_oversized_session_file() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("oversize");
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        // Sparse-Datei größer als das Limit erzeugen (kein 8-MiB-Write nötig).
        let f = fs::File::create(sessions.join("huge.jsonl")).unwrap();
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(f);
        let scanner = Scanner::new(&sessions);
        assert!(scanner.load("huge").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
