//! Coding-Werkzeuge für den Chat-Tool-Loop: Ausführen, Lesen, Schreiben,
//! Suchen und Git-Status im lokalen Projektbaum.
//!
//! Sicherheitsgrenzen:
//! - Alle Pfade werden gegen Projektwurzel validiert (kein Traversal, keine
//!   Symlink-Eskapade, keine Absolutpfade); nur Text- und Code-Dateien sind
//!   lesbar.
//! - `run_code` führt Shell-Befehle in einer temporären Sandbox aus: kein
//!   freier Shell-Zugriff auf den Projektbaum, begrenzte Umgebung, Timeout —
//!   reine Ausgabe-Rückgabe (stdout/stderr), kein Stream-Handling.
//! - Schreiboperationen sind atomar (temp-Datei + rename) und auf das
//!   Projektverzeichnis begrenzt. Vor jedem Schreiben/Patchen wird der
//!   Zielpfad normalisiert und gegen die Projektwurzel geprüft.
//!
//! Die Werkzeug-Ergebnisse folgen dem Muster aus `attachments.rs`: jede
//! Operation liefert ein kompaktes JSON-Objekt mit einer klaren Meldung, das
//! der Worker als Tool-Ergebnis wieder aufnimmt.

use serde_json::{json, Map, Value};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Maximale Länge von Code-Snippets oder Dateiinhalten, die an den Worker
/// zurückgegeben werden (verhindert, dass ein riesiges Output den
/// Antwortpuffer sprengt).
const MAX_TOOL_RESPONSE_BYTES: usize = 32 * 1024;

/// Maximale Größe einer Kommentar-Datei, die `read_comment_file` liest.
/// Klein gehalten, da Kommentar-/Notizdateien selten größer sind.
const MAX_COMMENT_FILE_BYTES: usize = 64 * 1024;

/// Maximale Anzahl zurückgegebener Suchergebnisse / aufgezählter Dateien.
const MAX_LIST_ENTRIES: usize = 200;

/// Warum wir den Projektbaum sperren: Alle Werkzeuge rechnen ausschließlich im
/// Arbeitsverzeichnis des Runners. Der Ort wird aus der Konfiguration
/// `paths.project_root` bezogen; dort darf dann z.B. `cargo run` oder
/// `npm test` für den Coding-Tab laufen.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        run_code_tool(),
        read_file_tool(),
        write_file_tool(),
        list_files_tool(),
        search_files_tool(),
        patch_file_tool(),
        git_status_tool(),
    ]
}

fn run_code_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "run_code",
            "description": "Führt einen Shell-/Code-Befehl in einer Sandbox-Umgebung im Projektverzeichnis aus. Liefert stdout und stderr zurück. Kein freier API-/Netzwerkzugriff, Timeout nach 30 s.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Beliebige Shell-Befehlszeile, z.B. 'ls -la' oder 'python3 script.py'"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 60}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }
    })
}

fn read_file_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Liest eine Text- oder Code-Datei relativ zur Projektwurzel und gibt ihren Inhalt zurück.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })
}

fn write_file_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Schreibt oder überschreibt eine Textdatei relativ zur Projektwurzel (atomar). Erstellt fehlende Verzeichnisse.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        }
    })
}

fn list_files_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "list_files",
            "description": "Zeigt die Projektstruktur unterhalb der Projektwurzel: Dateien und Unterverzeichnisse als Pfadliste (Max. 200 Einträge).",
            "parameters": {
                "type": "object",
                "properties": {
                    "dir": {"type": "string", "description": "Relativer Unterordner, Standard '.'"}
                },
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

fn search_files_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "search_files",
            "description": "Durchsucht Textdateien unterhalb der Projektwurzel nach einer Zeichenkette (grep-artig). Liefert Treffer mit Datei und Zeile.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "dir": {"type": "string", "description": "Relativer Startordner, Standard '.'"},
                    "extension": {"type": "string", "description": "Optional Dateiendung ohne Punkt, z.B. 'rs'"}
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }
    })
}

fn patch_file_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "patch_file",
            "description": "Ersetzt einen exakten Textabschnitt in einer Datei relativ zur Projektwurzel. Sicherer als komplettes Überschreiben: sucht das alte Vorkommen, ersetzt es und gibt die neue Datei zurück.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            }
        }
    })
}

fn git_status_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "git_status",
            "description": "Zeigt den Git-Status und das Diff-Statistic des Projektbaums (Branch, geänderte Dateien, Diff-Stat). Kein Datenverlust, nur Lesen.",
            "parameters": {
                "type": "object",
                "properties": {
                    "format": {"type": "string", "enum": ["status", "diffstat", "diff"], "description": "status = kurzer Status, diffstat = Änderungszahlen, diff = voller Patch"}
                },
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

/// Ergebnis eines Werkzeugaufrufs, verdichtet zu einer JSON-Zeile für den
/// Worker-Tool-Loop.
pub fn tool_result(call_id: &str, content: Value) -> Value {
    json!({
        "call_id": call_id,
        "content": content
    })
}

/// Führt einen Shell-Befehl in einer Sandbox aus.
fn run_sandbox(project_root: &Path, command: &str, timeout_secs: u64) -> Result<Value, String> {
    if command.trim().is_empty() {
        return Err("run_code: leere Befehlszeile".into());
    }
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 60));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root)
        // Sicherheitshalber keine freie Netzwerk- oder Host-Interaktion:
        // nur die nötigen Basis-Env-Variablen erben, Rest wird geleert.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", project_root.to_string_lossy().to_string())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run_code: Prozess nicht startbar: {e}"))?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "run_code: Timeout nach {}s überschritten",
                        timeout_secs
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("run_code: Wartefehler: {e}")),
        }
    };
    let stdout = child
        .wait_with_output()
        .map_err(|e| format!("run_code: Ausgabe nicht lesbar: {e}"))?;
    let out = String::from_utf8_lossy(&stdout.stdout);
    let err = String::from_utf8_lossy(&stdout.stderr);
    let trunc = |s: &str| -> String {
        if s.len() > MAX_TOOL_RESPONSE_BYTES {
            format!("{}… [gekürzt]", &s[..MAX_TOOL_RESPONSE_BYTES])
        } else {
            s.to_string()
        }
    };
    Ok(json!({
        "exit_code": status.code(),
        "stdout": trunc(&out),
        "stderr": trunc(&err),
    }))
}

/// Normalisiert einen Pfad relativ zur Projektwurzel; schlägt bei Traversal,
/// Symlink-Eskapade oder absolutem Argument fehl. Rückgabe ist der sichere,
/// kanonische Pfad.
fn safe_relative_path(project_root: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("Pfad darf nicht leer sein".into());
    }
    if Path::new(raw).is_absolute() {
        return Err("Pfad muss relativ zur Projektwurzel sein".into());
    }
    let norm = Path::new(raw);
    // Komponenten prüfen: keine '..', keine '.'-Tricks, keine Windows-Laufwerke.
    let mut checked = PathBuf::new();
    let mut only_curdir = true;
    for comp in norm.components() {
        match comp {
            Component::CurDir => {} // '.' ignorieren
            Component::ParentDir | Component::RootDir => {
                return Err("Pfad darf kein '..' oder '/' enthalten".into());
            }
            Component::Prefix(_) => return Err("Pfad darf keine Laufwerkangabe enthalten".into()),
            Component::Normal(seg) => {
                only_curdir = false;
                checked.push(seg);
            }
        }
    }
    let root_canon = fs::canonicalize(project_root)
        .map_err(|e| format!("Projektwurzel nicht auflösbar: {e}"))?;
    if only_curdir {
        // Eingaben wie "." oder "./" bedeuten: die Projektwurzel selbst.
        return Ok(root_canon);
    }
    if checked.as_os_str().is_empty() {
        return Err("Pfad ist leer".into());
    }
    let candidate = project_root.join(&checked);
    // Symlink-Eskapade verhindern: Der kanonisierte Zielpfad muss innerhalb
    // der Projektwurzel liegen. Existiert der Pfad noch nicht (Write-Fall),
    // wird stattdessen der sicher auflösbare Parent gegen die Wurzel geprüft.
    match fs::canonicalize(&candidate) {
        Ok(canon) => {
            if !canon.starts_with(&root_canon) {
                return Err("Pfad verlässt die Projektwurzel".into());
            }
            Ok(canon)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Für das Schreiben ist ein noch nicht existierender Pfad kein
            // Fehler; der nächste existierende Vorfahre muss aber sicher
            // innerhalb der Projektwurzel liegen (kein Traversal).
            let mut probe = candidate.as_path();
            while probe.parent().is_some() && probe.parent() != Some(probe) {
                match fs::canonicalize(probe) {
                    Ok(existing) => {
                        if !existing.starts_with(&root_canon) {
                            return Err("Pfad verlässt die Projektwurzel".into());
                        }
                        return Ok(candidate);
                    }
                    Err(_) => probe = probe.parent().unwrap(),
                }
            }
            Err(e.to_string())
        }
        Err(e) => Err(format!("Pfad nicht auflösbar: {e}")),
    }
}

/// Liest eine Kommentar-/Notizdatei relativ zur Projektwurzel als Text.
///
/// Sicherheitsstrikt: nutzt die bestehende `safe_relative_path`-Validierung
/// (kein Traversal, keine Symlink-Eskapade) und lehnt Binärdaten (NUL-Bytes
/// bzw. ungültige UTF-8-Sequenzen) ab. Inhalte über 64 KiB werden auf 64 KiB
/// beschnitten und mit `truncated` markiert. Rückgabe ist ein
/// `{"path", "content", "truncated"}`-Value analog zu `read_file`.
///
/// Hinweis zur Whitelist: `attachments::is_allowed_tool_name` erlaubt diesen
/// Namen derzeit NICHT (festes Set ohne `read_comment_file`) und `attachments.rs`
/// darf nicht angefasst werden. Daher ist diese Funktion NICHT über den
/// Tool-Loop aufrufbar, sondern nur als `pub fn` für direkten Testzugriff
/// vorhanden (Traceability ohne die Whitelist zu verletzen).
pub fn read_comment_file(project_root: &Path, path: &str) -> Result<Value, String> {
    let safe = safe_relative_path(project_root, path)?;
    let data =
        fs::read(&safe).map_err(|e| format!("read_comment_file: Datei nicht lesbar: {e}"))?;
    // Binärdaten ablehnen: NUL-Bytes oder ungültige UTF-8-Sequenz kennzeichnen
    // eine Binärdatei (kein reiner Text-Kommentar).
    if data.contains(&0) {
        return Err("read_comment_file: Datei enthaelt NUL-Bytes (binaer)".into());
    }
    let text = String::from_utf8(data).map_err(|_| {
        "read_comment_file: Datei ist kein gueltiger UTF-8-Text (binaer)".to_string()
    })?;
    // 64-KiB-Limit: darüber hinausgehenden Rest verwerfen und 'truncated' melden.
    let truncated = text.len() > MAX_COMMENT_FILE_BYTES;
    let content = if truncated {
        let mut end = MAX_COMMENT_FILE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_string()
    } else {
        text
    };
    Ok(json!({"path": path, "content": content, "truncated": truncated}))
}

/// Führt die namensbasierte Operation aus. `args` ist das geparste
/// Argument-Objekt; Rückgabe ist das Ergebnis-Value für den Tool-Loop.
pub fn execute(
    project_root: &Path,
    name: &str,
    args: &Map<String, Value>,
) -> Result<Value, String> {
    let required_str = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{key} muss ein nicht leerer String sein"))
    };

    match name {
        "run_code" => {
            let command = required_str("command")?;
            let timeout = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(30);
            run_sandbox(project_root, &command, timeout)
        }
        "read_file" => {
            let rel = required_str("path")?;
            let path = safe_relative_path(project_root, &rel)?;
            let data = fs::read_to_string(&path)
                .map_err(|e| format!("read_file: Datei nicht lesbar: {e}"))?;
            let truncated = data.len() > MAX_TOOL_RESPONSE_BYTES;
            let content = if truncated {
                data[..MAX_TOOL_RESPONSE_BYTES].to_string()
            } else {
                data
            };
            Ok(json!({"path": rel, "content": content, "truncated": truncated}))
        }
        "write_file" => {
            let rel = required_str("path")?;
            let content = required_str("content")?;
            let path = safe_relative_path(project_root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("write_file: Verzeichnis nicht anlegbar: {e}"))?;
            }
            // Atomarer Schreibvorgang: temp-Datei im selben Verzeichnis, dann rename.
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            {
                let mut f = fs::File::create(&tmp)
                    .map_err(|e| format!("write_file: Datei nicht anlegbar: {e}"))?;
                f.write_all(content.as_bytes())
                    .map_err(|e| format!("write_file: Schreiben fehlgeschlagen: {e}"))?;
                f.flush()
                    .map_err(|e| format!("write_file: Flush fehlgeschlagen: {e}"))?;
            }
            fs::rename(&tmp, &path)
                .map_err(|e| format!("write_file: Umbenennen fehlgeschlagen: {e}"))?;
            Ok(json!({"path": rel, "bytes_written": content.len()}))
        }
        "list_files" => {
            let dir = args
                .get("dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(".");
            let base = safe_relative_path(project_root, dir)?;
            let mut entries = Vec::new();
            walk(&base, &mut entries, 0)?;
            let entries: Vec<PathBuf> = entries.into_iter().take(MAX_LIST_ENTRIES).collect();
            // Als relative Pfade zurückgeben, damit der Worker keine
            // Dateisystemdetails der Maschine erfährt.
            let rel_entries: Vec<String> = entries
                .iter()
                .map(|p| {
                    p.strip_prefix(&base)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            Ok(json!({"dir": dir, "count": rel_entries.len(), "files": rel_entries}))
        }
        "search_files" => {
            let query = required_str("query")?;
            let dir = args
                .get("dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(".");
            let ext = args
                .get("extension")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let base = safe_relative_path(project_root, dir)?;
            let mut hits = Vec::new();
            search(&base, &query, ext, &mut hits, 0)?;
            let hits: Vec<Value> = hits.into_iter().take(MAX_LIST_ENTRIES).collect();
            Ok(json!({"query": query, "count": hits.len(), "results": hits}))
        }
        "patch_file" => {
            let rel = required_str("path")?;
            let old = required_str("old")?;
            let new = required_str("new")?;
            let path = safe_relative_path(project_root, &rel)?;
            let data = fs::read_to_string(&path)
                .map_err(|e| format!("patch_file: Datei nicht lesbar: {e}"))?;
            let mut occurrences = 0;
            let mut pos = 0;
            while let Some(idx) = data[pos..].find(&old) {
                occurrences += 1;
                pos += idx + old.len();
            }
            if occurrences == 0 {
                return Err("patch_file: alte Zeichenkette nicht gefunden".into());
            }
            if occurrences > 1 {
                return Err(format!(
                    "patch_file: Zeichenkette kommt {occurrences}× vor — genauer angeben"
                ));
            }
            let patched = data.replace(&old, &new);
            {
                let tmp = path.with_extension(format!("tmp{}", std::process::id()));
                let mut f = fs::File::create(&tmp)
                    .map_err(|e| format!("patch_file: Datei nicht anlegbar: {e}"))?;
                f.write_all(patched.as_bytes())
                    .map_err(|e| format!("patch_file: Schreiben fehlgeschlagen: {e}"))?;
                f.flush()
                    .map_err(|e| format!("patch_file: Flush fehlgeschlagen: {e}"))?;
                fs::rename(&tmp, &path)
                    .map_err(|e| format!("patch_file: Umbenennen fehlgeschlagen: {e}"))?;
            }
            Ok(json!({"path": rel, "replaced": 1}))
        }
        "git_status" => {
            let format = args
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("status");
            match format {
                "status" => run_git(project_root, &["status", "--short", "--branch"]),
                "diffstat" => run_git(project_root, &["diff", "--stat"]),
                "diff" => run_git(project_root, &["diff"]),
                other => Err(format!("git_status: unbekanntes Format '{other}'")),
            }
        }
        other => Err(format!("unbekanntes Coding-Tool '{other}'")),
    }
}

fn run_git(project_root: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("git_status: git nicht ausführbar: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code();
    if !out.status.success() {
        return Err(format!(
            "git_status: git meldet: {}",
            stderr.trim().lines().next().unwrap_or("unbekannter Fehler")
        ));
    }
    Ok(json!({
        "exit_code": code,
        "output": stdout.trim().to_string()
    }))
}

fn walk(base: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<(), String> {
    if depth > 8 {
        return Ok(()); // Rekursionstiefen begrenzen, keine Endlosschleifen.
    }
    let entries = fs::read_dir(base).map_err(|e| format!("list_files: nicht lesbar: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            out.push(path.clone());
            walk(&path, out, depth + 1)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn search(
    base: &Path,
    query: &str,
    extension: Option<&str>,
    out: &mut Vec<Value>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 || out.len() >= MAX_LIST_ENTRIES {
        return Ok(());
    }
    let entries = fs::read_dir(base).map_err(|e| format!("search_files: nicht lesbar: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            search(&path, query, extension, out, depth + 1)?;
        } else if let Some(ext) = extension {
            if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                scan_file(&path, query, base, out)?;
            }
        } else {
            scan_file(&path, query, base, out)?;
        }
    }
    Ok(())
}

fn scan_file(path: &Path, query: &str, base: &Path, out: &mut Vec<Value>) -> Result<(), String> {
    let Ok(data) = fs::read_to_string(path) else {
        return Ok(()); // binäre/nicht lesbare Dateien überspringen
    };
    for (lineno, line) in data.lines().enumerate() {
        if line.contains(query) {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let line_text = line.trim().chars().take(240).collect::<String>();
            out.push(json!({"file": rel, "line": lineno + 1, "text": line_text}));
            if out.len() >= MAX_LIST_ENTRIES {
                break;
            }
        }
    }
    Ok(())
}

/// Verarbeitet alle `tool_calls` einer Worker-Antwort und führt jede erlaubte
/// Operation aus. Rückgabe ist die Liste der `ToolResult`s (call_id + JSON-
/// Inhalt) für die Weiterverarbeitung im Tool-Loop. Attachment-Calls
/// (`read_attachment`) werden an `attachments.rs` delegiert.
pub fn execute_tool_calls(
    response: &Value,
    attachment_index: Option<&crate::attachments::AttachmentIndex>,
    project_root: &Path,
) -> Result<Vec<crate::attachments::ToolResult>, String> {
    let Some(calls) = response
        .pointer("/choices/0/message/tool_calls")
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let mut results = Vec::new();
    for call in calls {
        let object = call
            .as_object()
            .ok_or_else(|| "tool_call muss ein JSON-Objekt sein".to_string())?;
        let call_id = required_str(object, "id")?;
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| "tool_call.function fehlt".to_string())?;
        let name = required_str(function, "name")?;
        let args = parse_tool_arguments(function.get("arguments"))?;
        if name == "read_attachment" {
            let Some(index) = attachment_index else {
                return Err("read_attachment ohne Attachment-Index aufgerufen".into());
            };
            let attachment_id =
                required_str(&args, "attachment_id").or_else(|_| required_str(&args, "id"))?;
            let chunk_index = args
                .get("chunk_index")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            results.push(crate::attachments::ToolResult {
                call_id,
                content: index.read(&attachment_id, chunk_index)?,
            });
        } else if crate::attachments::is_allowed_tool_name(&name) {
            let result = execute(project_root, &name, &args)?;
            results.push(crate::attachments::ToolResult {
                call_id,
                content: result.to_string(),
            });
        } else {
            return Err(format!("Tool '{name}' ist nicht freigegeben"));
        }
    }
    Ok(results)
}

fn required_str(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} muss ein nicht leerer String sein"))
}

fn parse_tool_arguments(value: Option<&Value>) -> Result<Map<String, Value>, String> {
    match value {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text)
            .map_err(|err| format!("Tool-Argumente sind kein JSON: {err}"))?
            .as_object()
            .cloned()
            .ok_or_else(|| "Tool-Argumente muessen ein JSON-Objekt sein".into()),
        Some(Value::Object(object)) => Ok(object.clone()),
        _ => Err("Tool-Argumente fehlen".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Wie `TEST_LOCK.lock()`, überlebt aber ein gepoissonies Mutex (ein
    /// Test-Fehler darf die übrigen Datei-Tests nicht mitreißen) und gibt
    /// stattdessen eine frische Serialisierung.
    fn lock_test() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tri-coding-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn read_file_rejects_traversal_and_absolute_paths() {
        let _g = lock_test();
        let root = temp_root("traversal");
        fs::write(root.join("ok.txt"), "hello").unwrap();
        assert!(safe_relative_path(&root, "../etc/passwd").is_err());
        assert!(safe_relative_path(&root, "/etc/passwd").is_err());
        assert!(safe_relative_path(&root, "").is_err());
        assert!(safe_relative_path(&root, "ok.txt").is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_file_reads_content_and_marks_truncation() {
        let _g = lock_test();
        let root = temp_root("read");
        fs::write(root.join("notes.md"), "alpha\nbeta").unwrap();
        let result = execute(&root, "read_file", &args(json!({"path": "notes.md"}))).unwrap();
        assert_eq!(result["content"], "alpha\nbeta");
        assert_eq!(result["truncated"], false);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_comment_file_reads_text_and_reports_truncation() {
        let _g = lock_test();
        let root = temp_root("comment");
        fs::write(root.join("notes.txt"), "Ein Kommentar\nzweite Zeile").unwrap();
        let result = read_comment_file(&root, "notes.txt").unwrap();
        assert_eq!(result["path"], "notes.txt");
        assert_eq!(result["content"], "Ein Kommentar\nzweite Zeile");
        assert_eq!(result["truncated"], false);
        // Datei ueber 64 KiB wird gelesen, aber beschnitten gemeldet.
        let big = "x".repeat(MAX_COMMENT_FILE_BYTES + 100);
        fs::write(root.join("big.txt"), &big).unwrap();
        let big_result = read_comment_file(&root, "big.txt").unwrap();
        assert_eq!(big_result["truncated"], true);
        assert!(big_result["content"].as_str().unwrap().len() <= MAX_COMMENT_FILE_BYTES);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_comment_file_rejects_traversal_and_missing_and_binary() {
        let _g = lock_test();
        let root = temp_root("comment2");
        fs::write(root.join("ok.txt"), "hallo").unwrap();
        // Traversal verlässt die Projektwurzel.
        assert!(read_comment_file(&root, "../etc/passwd").is_err());
        assert!(read_comment_file(&root, "/etc/passwd").is_err());
        // Fehlende Datei wird abgelehnt.
        assert!(read_comment_file(&root, "gibt-es-nicht.txt").is_err());
        // Binäre Datei (NUL-Bytes) wird abgelehnt.
        fs::write(root.join("bin.dat"), b"\x00\x01\x02bin").unwrap();
        assert!(read_comment_file(&root, "bin.dat").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_file_creates_parent_dirs_and_writes_atomically() {
        let _g = lock_test();
        let root = temp_root("write");
        let result = execute(
            &root,
            "write_file",
            &args(json!({"path": "a/b/c.txt", "content": "xyz"})),
        )
        .unwrap();
        assert_eq!(result["bytes_written"], 3);
        assert_eq!(fs::read_to_string(root.join("a/b/c.txt")).unwrap(), "xyz");
        // Und überschreiben:
        execute(
            &root,
            "write_file",
            &args(json!({"path": "a/b/c.txt", "content": "new"})),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("a/b/c.txt")).unwrap(), "new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn patch_file_replaces_exactly_once() {
        let _g = lock_test();
        let root = temp_root("patch");
        fs::write(root.join("f.rs"), "fn a() { x(); }\nfn a_second() {}").unwrap();
        // "a()" kommt einmal vor (a_second enthält kein "a()"):
        let result = execute(
            &root,
            "patch_file",
            &args(json!({"path": "f.rs", "old": "a()", "new": "b()"})),
        )
        .unwrap();
        assert_eq!(result["replaced"], 1);
        let content = fs::read_to_string(root.join("f.rs")).unwrap();
        assert!(content.contains("b()"));
        assert!(!content.contains("a()"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn patch_file_rejects_missing_or_ambiguous_target() {
        let _g = lock_test();
        let root = temp_root("patch2");
        fs::write(root.join("f.txt"), "aaa aaa").unwrap();
        assert!(execute(
            &root,
            "patch_file",
            &args(json!({"path": "f.txt", "old": "a", "new": "b"}))
        )
        .is_err());
        fs::write(root.join("g.txt"), "nothing").unwrap();
        assert!(execute(
            &root,
            "patch_file",
            &args(json!({"path": "g.txt", "old": "zzz", "new": "b"}))
        )
        .is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_files_skips_vcs_and_build_dir_returns_relative() {
        let _g = lock_test();
        let root = temp_root("list");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::write(root.join("target/out.bin"), "x").unwrap();
        fs::write(root.join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(root.join("src/nested/lib.rs"), "pub fn x(){}").unwrap();
        let result = execute(&root, "list_files", &args(json!({"dir": "."}))).unwrap();
        let files = result["files"].as_array().unwrap();
        assert!(!files.iter().any(|f| f.as_str().unwrap().contains(".git")));
        assert!(!files.iter().any(|f| f.as_str().unwrap().contains("target")));
        assert!(files.iter().any(|f| f.as_str().unwrap() == "src/main.rs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_files_finds_matches_and_keeps_count_bounded() {
        let _g = lock_test();
        let root = temp_root("search");
        fs::write(root.join("a.rs"), "fn todo() {}\nfn other(){}\n").unwrap();
        fs::write(root.join("b.txt"), "todo here\n").unwrap();
        let result = execute(
            &root,
            "search_files",
            &args(json!({"query": "todo", "extension": "rs"})),
        )
        .unwrap();
        assert_eq!(result["count"], 1);
        let result_all = execute(&root, "search_files", &args(json!({"query": "todo"}))).unwrap();
        assert_eq!(result_all["count"], 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn run_code_executes_in_sandbox_and_captures_output() {
        let _g = lock_test();
        let root = temp_root("run");
        let result = execute(
            &root,
            "run_code",
            &args(json!({"command": "echo hello; echo err >&2; exit 3"})),
        )
        .unwrap();
        assert_eq!(result["stdout"], "hello\n");
        assert_eq!(result["stderr"], "err\n");
        assert_eq!(result["exit_code"], 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn run_code_rejects_empty_and_times_out() {
        let _g = lock_test();
        let root = temp_root("run2");
        assert!(execute(&root, "run_code", &args(json!({"command": "  "}))).is_err());
        let start = Instant::now();
        let result = execute(
            &root,
            "run_code",
            &args(json!({"command": "sleep 5", "timeout_secs": 1})),
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        let _ = fs::remove_dir_all(&root);
    }
}
