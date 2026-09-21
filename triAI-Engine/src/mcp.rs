//! Schlanker MCP (Model Context Protocol)-Client für tri-ai-runner.
//!
//! GoOze/Goose hat MCP als Erweiterungs-Standard; dieser Strang schließt die
//! Lücke mit einem *dependency-armen* JSON-RPC-2.0-Client über den Stdio-
//! Transport. Keine MCP-Lib, kein Netzwerk-Stack von außen — wir sprechen den
//! MCP-Server als Unterprozess über JSON-Lines auf stdin/stdout an (das ist
//! der Standard-Transport des MCP-Protokolls).
//!
//! Sicherheitsgrenze: Es werden ausschliesslich Tools ausgeführt, deren Name
//! über `attachments::is_allowed_tool_name` freigegeben ist. Jede andere
//! Antwort des MCP-Servers (tool_call mit unbekanntem Namen) wird abgelehnt.
//! Der MCP-Server selbst wird bewusst NUR bei expliziter Konfiguration
//! gestartet (siehe `McpConfig::default()`, hier deaktiviert); ohne
//! Konfiguration liefern alle Funktionen eine leere Tool-Liste.

use serde_json::{json, Map, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    time::Duration,
};

/// Parameter eines einzelnen MCP-Tools.
pub struct McpTools {
    /// Name -> (description, required-Feldliste)
    pub items: Vec<(String, String, Vec<String>)>,
}

/// Konfiguration eines MCP-Servers. Standardmäßig deaktiviert.
#[derive(Clone, Debug)]
pub struct McpConfig {
    /// Optionaler Befehl des MCP-Servers (Pfad oder in PATH), z.B.
    /// `Some("my-mcp-server".into())`. `None` = MCP ist aus.
    pub command: Option<PathBuf>,
    /// Timeout für initialize + tools/list + einen einzelnen tool/call.
    pub timeout: Duration,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            command: None,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Parst die `jsonrpc`-Feld-Definition eines MCP-Tools in das leichtgewichtige
/// Format der GUI-Tool-Liste (`name`, `description`, `parameters`, `required`).
pub fn mcp_tool_definitions(config: &McpConfig) -> Vec<Value> {
    let Ok(tools) = list_tools(config) else {
        return Vec::new();
    };
    tools
        .items
        .into_iter()
        .filter(|(name, _, _)| crate::attachments::is_allowed_tool_name(name))
        .map(|(name, description, required)| {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": Value::Object(Map::new()),
                        "required": required
                    },
                }
            })
        })
        .collect()
}

/// Listet verfügbare MCP-Tools über `tools/list` auf. Leerer Ergebnis-Satz,
/// wenn kein Server konfiguriert ist oder das Listen fehlschlägt (nie hart).
pub fn list_tools(config: &McpConfig) -> Result<McpTools, String> {
    let mut session = McpSession::spawn(config)?;
    session.initialize()?;
    let tools = session.list_tools()?;
    let _ = session.child.kill();
    let _ = session.child.wait();
    Ok(tools)
}

/// Führt ein einzelnes freigegebenes MCP-Tool mit `arguments` aus und liefert
/// den Ergebnis-JSON (das LLM benutzt dieses Tool über den Chat-Tool-Loop).
pub fn call_tool(
    config: &McpConfig,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<Value, String> {
    // Whitelist: nur erlaubte Tool-Namen werden überhaupt an den MCP-Server
    // durchgereicht — ein unbekannter Name wird abgelehnt, er darf nie an den
    // Server gelangen.
    if !crate::attachments::is_allowed_tool_name(name) {
        return Err(format!("Tool '{name}' ist nicht freigegeben"));
    }
    let mut session = McpSession::spawn(config)?;
    session.initialize()?;
    let result = session.call_tool(name, arguments)?;
    let _ = session.child.kill();
    let _ = session.child.wait();
    Ok(result)
}

/// Interne Prozess-Sitzung eines MCP-Servers über Stdio.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    /// Empfänger für stdout-Zeilen des Kindes (gefüllt von einem Reader-Thread).
    rx: std::sync::mpsc::Receiver<String>,
    next_id: u64,
    timeout: Duration,
}

impl McpSession {
    fn spawn(config: &McpConfig) -> Result<Self, String> {
        let Some(command) = config.command.as_deref() else {
            return Err("MCP ist nicht konfiguriert".into());
        };
        let mut child = Command::new(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("MCP-Server nicht startbar: {e}"))?;
        let stdin = child.stdin.take().ok_or("MCP-Server stdin fehlt")?;
        let stdout = child.stdout.take().ok_or("MCP-Server stdout fehlt")?;
        // Reader-Thread: liest jede stdout-Zeile und sendet sie an den Channel.
        // `read_line` blockiert dabei innerhalb des Threads, aber `recv_timeout`
        // auf der Empfängerseite macht die Gesamtsitzung timeout-fähig.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // EOF oder Fehler -> Thread endet
                    Ok(_) => {
                        if tx.send(line.clone()).is_err() {
                            break; // Empfänger weg -> Thread endet
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            rx,
            next_id: 1,
            timeout: config.timeout,
        })
    }

    fn next_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "tri-ai-runner", "version": "0.1"}}
        });
        let _response = self.request(request)?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<McpTools, String> {
        let id = self.next_id();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}});
        let response = self.request(request)?;
        let array = response
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "MCP tools/list lieferte kein result.tools-Array".to_string())?;
        let mut items = Vec::new();
        for tool in array {
            let name = tool.pointer("/name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let description = tool
                .pointer("/description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let required = tool
                .pointer("/inputSchema/required")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            items.push((name.to_string(), description, required));
        }
        Ok(McpTools { items })
    }

    fn call_tool(&mut self, name: &str, arguments: &Map<String, Value>) -> Result<Value, String> {
        let id = self.next_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let response = self.request(request)?;
        // MCP-Ergebnisse liegen in result.content als Array von Text-Bausteinen.
        let content = response
            .pointer("/result/content")
            .and_then(Value::as_array)
            .ok_or_else(|| "MCP tools/call lieferte kein result.content-Array".to_string())?;
        let mut text = String::new();
        for part in content {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if let Some(is_error) = response.pointer("/result/isError").and_then(Value::as_bool) {
            if is_error && text.is_empty() {
                return Err("MCP-Tool meldete einen Fehler (leer)".into());
            }
        }
        Ok(json!({"content": text}))
    }

    fn request(&mut self, request: Value) -> Result<Value, String> {
        let payload = request.to_string();
        self.stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("MCP-request nicht schreibbar: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|e| format!("MCP-request (newline) nicht schreibbar: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("MCP-request (flush) fehlgeschlagen: {e}"))?;

        let wanted_id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        loop {
            // `recv_timeout` gibt nach Ablauf des MCP-Timeout's auf, auch wenn
            // der Reader-Thread weiter blockiert — genau dort, wo das
            // frühere blockierende `read_line` das Timeout vereitelte.
            let line = self
                .rx
                .recv_timeout(self.timeout)
                .map_err(|_| "MCP-Antwort-Timeout oder stdout geschlossen".to_string())?;
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue; // keine JSON-Line (Notifikation o.ä.) -> überspringen
            };
            // IDs vergleichen — MCP-Server antworten teils mit String-,
            // teils mit Integer-IDs. Gleiche beide Formen ab, sonst verwerfen
            // wir gültige Antworten und laufen ins Timeout.
            let is_match = match msg.get("id") {
                Some(Value::String(s)) => s == &wanted_id,
                Some(Value::Number(n)) => {
                    // Anfrage-IDs sind fortlaufend ("1","2",...): numerisch
                    // gegen die gewünschte Zeichenform gleichen.
                    n.to_string() == wanted_id
                }
                _ => false,
            };
            if is_match {
                if let Some(err) = msg.get("error") {
                    let message = err
                        .pointer("/message")
                        .and_then(Value::as_str)
                        .unwrap_or("unbekannter MCP-Fehler");
                    return Err(format!("MCP-Server-Fehler: {message}"));
                }
                return Ok(msg);
            }
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Leere Tool-Liste, wenn MCP deaktiviert ist (Standard).
pub fn enabled(config: &McpConfig) -> bool {
    config.command.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schreibt einen Stub-MCP-Server als Shell-Skript und liefert dessen Pfad.
    fn stub_server(script: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tri-mcp-stub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stub-mcp.sh");
        std::fs::write(&path, script).unwrap();
        // chmod +x
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn disabled_mcp_returns_no_tools() {
        let cfg = McpConfig::default();
        assert!(!enabled(&cfg));
        assert!(mcp_tool_definitions(&cfg).is_empty());
    }

    #[test]
    fn stub_server_lists_and_calls_tool() {
        // Ein Stub, der initialize -> {}, tools/list -> {tools: [...]} und
        // tools/call -> {content: "ok"} auf stdin reagiert und pro Zeile eine
        // JSON-Line auf stdout schreibt.
        let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"*\([0-9]\+\)"*.*/\1/p')
  case "$line" in
    *"tools/list"*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"read_file\",\"description\":\"read a file\",\"inputSchema\":{\"type\":\"object\",\"required\":[\"path\"]}}]}}";;
    *"tools/call"*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello mcp\"}]}}";;
    *"initialize"*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}";;
    *) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}";;
  esac
done"#;
        let script_path = stub_server(script);
        let cfg = McpConfig {
            command: Some(script_path.clone()),
            timeout: Duration::from_secs(5),
        };
        let tools = list_tools(&cfg).unwrap();
        assert_eq!(tools.items.len(), 1);
        assert_eq!(tools.items[0].0, "read_file");
        let args = serde_json::json!({"path": "x"})
            .as_object()
            .unwrap()
            .clone();
        let result = call_tool(&cfg, "read_file", &args).unwrap();
        assert_eq!(result["content"], "hello mcp");
        let _ = std::fs::remove_dir_all(script_path.parent().unwrap());
    }

    #[test]
    fn unknown_tool_name_is_rejected_before_spawning() {
        // Auch ohne konfigurierten Server darf ein nicht freigegebener Name
        // NICHT loslaufen — der Fehler kommt vor dem Spawn.
        let cfg = McpConfig {
            command: Some(PathBuf::from("/bin/false")),
            timeout: Duration::from_secs(1),
        };
        let args = serde_json::json!({}).as_object().unwrap().clone();
        assert!(call_tool(&cfg, "nicht_freigegeben", &args).is_err());
    }
}
