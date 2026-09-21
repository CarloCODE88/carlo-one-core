//! Kleine, dependency-freie JSONL-Ereignisprotokollierung.
//!
//! Sie ist bewusst best-effort: ein nicht beschreibbares Log darf nie den
//! lokalen Modellbetrieb blockieren. Prompts und Modellantworten werden nicht
//! geloggt, nur technische Lebenszyklusdaten.

use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static EVENT_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static EVENT_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Legt das Prozessziel für technische Ereignisse fest. Wiederholte Aufrufe
/// mit demselben Pfad sind erlaubt; ein späterer Pfadwechsel wird abgelehnt.
pub fn configure(path: impl Into<PathBuf>) -> io::Result<()> {
    let path = path.into();
    if let Some(existing) = EVENT_LOG_PATH.get() {
        return if existing == &path {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Eventlog ist bereits auf '{}' konfiguriert",
                    existing.display()
                ),
            ))
        };
    }
    EVENT_LOG_PATH.set(path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Eventlog wurde gleichzeitig konfiguriert",
        )
    })
}

pub fn emit(event: &str, fields: Value) {
    let default_path = crate::config::PathConfig::default().event_log;
    let path = EVENT_LOG_PATH.get().unwrap_or(&default_path);
    let _ = emit_to(path, event, fields);
}

pub fn emit_to(path: &Path, event: &str, fields: Value) -> io::Result<()> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut safe = fields.clone();
    redact_secrets(&mut safe);
    let event_id = EVENT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let record = json!({
        "schema_version": EVENT_SCHEMA_VERSION,
        "event_id": event_id,
        "timestamp_ms": timestamp_ms,
        "event": event,
        "fields": safe
    });
    let line = serde_json::to_string(&record).map_err(io::Error::other)?;
    let lock = LOG_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    drop(lock);
    Ok(())
}

/// Entfernt/ersetzt bekannte Geheimnisfelder in einem JSON-Wert, bevor er in
/// ein Ereignisprotokoll geschrieben wird. Rekursive Maskierung; Schlüssel wie
/// `api_key`, `pin`, `token`, `password`, `secret` werden durch `***` ersetzt.
/// Reine Funktion — testbar.
fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let sensitive = {
                    let k = key.to_ascii_lowercase();
                    k.contains("api_key")
                        || k.contains("pin")
                        || k.contains("token")
                        || k.contains("password")
                        || k.contains("secret")
                };
                if sensitive {
                    *val = Value::String("***".to_string());
                } else {
                    redact_secrets(val);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_secrets(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_parseable_jsonl_event() {
        let path = std::env::temp_dir().join(format!("tri-ai-event-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        emit_to(&path, "worker_started", json!({"model": "fixture.gguf"})).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(value["event"], "worker_started");
        assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
        assert!(value["event_id"].as_u64().is_some());
        assert_eq!(value["fields"]["model"], "fixture.gguf");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn redact_masks_secret_fields_recursively() {
        let mut v = serde_json::json!({
            "model": "fixture",
            "api_key": "sk-abc",
            "nested": {"password": "pwd", "keep": "ok", "token_id": "tok"},
            "arr": [{"pin": "1234"}, {"secret": "s"}]
        });
        redact_secrets(&mut v);
        assert_eq!(v["api_key"], "***");
        assert_eq!(v["nested"]["password"], "***");
        assert_eq!(v["nested"]["keep"], "ok");
        assert_eq!(v["arr"][0]["pin"], "***");
        assert_eq!(v["arr"][1]["secret"], "***");
        assert_eq!(v["model"], "fixture");
    }

    #[test]
    fn emit_to_never_writes_secret_values() {
        let path = std::env::temp_dir().join(format!("tri-ai-redact-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        emit_to(
            &path,
            "chat_requested",
            json!({"model": "m", "api_key": "sk-leak", "prompt": "hallo"}),
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("sk-leak"),
            "Geheimnis darf nicht im Log erscheinen"
        );
        assert!(
            content.contains("\"***\""),
            "maskierter Wert muss sichtbar sein"
        );
        let _ = std::fs::remove_file(path);
    }
}
