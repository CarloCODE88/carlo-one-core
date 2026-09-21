//! Sicherheitsgrenze fuer explizit angehaengte Textdateien.
//!
//! Der Runner liest hier keine Pfade vom Dateisystem. Die GUI oder ein Client
//! muss den Inhalt bereits explizit mitsenden. Innerhalb eines Chat-Requests
//! existiert der Index nur temporaer im Speicher.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub const MAX_ATTACHMENTS: usize = 16;
pub const MAX_ATTACHMENT_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: usize = 1024 * 1024;
pub const MAX_TOOL_ROUNDS: usize = 4;
const CHUNK_BYTES: usize = 4096;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "json", "csv", "rs", "py", "js", "jsx", "ts", "tsx", "toml", "yaml",
    "yml", "html", "css", "sh", "c", "cc", "cpp", "h", "hpp", "go", "java", "kt", "swift",
];

const ALLOWED_MEDIA_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
    "application/x-ndjson",
    "application/toml",
    "application/yaml",
    "text/x-rust",
    "text/x-python",
    "text/javascript",
    "text/typescript",
    "text/html",
    "text/css",
    "text/x-shellscript",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentIndex {
    attachments: Vec<Attachment>,
    chunks: Vec<AttachmentChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attachment {
    id: String,
    name: String,
    media_type: Option<String>,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachmentChunk {
    attachment_id: String,
    chunk_index: usize,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
}

impl AttachmentIndex {
    pub fn from_openai_request(raw: &Value) -> Result<Option<Self>, String> {
        let Some(value) = raw.get("attachments") else {
            return Ok(None);
        };
        let values = value
            .as_array()
            .ok_or_else(|| "attachments muss ein Array sein".to_string())?;
        if values.is_empty() {
            return Ok(None);
        }
        if values.len() > MAX_ATTACHMENTS {
            return Err(format!("hoechstens {MAX_ATTACHMENTS} Attachments erlaubt"));
        }
        let mut seen = BTreeSet::new();
        let mut total_bytes = 0usize;
        let mut attachments = Vec::new();
        for (index, value) in values.iter().enumerate() {
            let object = value
                .as_object()
                .ok_or_else(|| "jedes Attachment muss ein JSON-Objekt sein".to_string())?;
            let name = required_string(object, "name")?;
            validate_name(&name)?;
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("attachment_{index}"));
            validate_id(&id)?;
            if !seen.insert(id.clone()) {
                return Err(format!("Attachment-ID '{id}' ist doppelt"));
            }
            let media_type = object
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|media_type| !media_type.is_empty())
                .map(str::to_owned);
            validate_type(&name, media_type.as_deref())?;
            let text = object
                .get("content")
                .or_else(|| object.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| "Attachment braucht ein Textfeld 'content' oder 'text'".to_string())?
                .to_owned();
            validate_text(&text)?;
            let bytes = text.len();
            if bytes > MAX_ATTACHMENT_BYTES {
                return Err(format!(
                    "Attachment '{name}' ueberschreitet das Groessenlimit"
                ));
            }
            total_bytes = total_bytes.saturating_add(bytes);
            if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
                return Err("Attachments ueberschreiten das Gesamtlimit".into());
            }
            attachments.push(Attachment {
                id,
                name,
                media_type,
                text,
            });
        }
        let chunks = attachments.iter().flat_map(chunk_attachment).collect();
        Ok(Some(Self {
            attachments,
            chunks,
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }

    pub fn system_message(&self) -> String {
        let mut message = String::from(
            "Angehaengte Dateien sind nur ueber das Tool read_attachment lesbar. Verweise auf Attachment-IDs, nicht auf Dateipfade.\n",
        );
        for attachment in &self.attachments {
            let media = attachment.media_type.as_deref().unwrap_or("text/plain");
            message.push_str(&format!(
                "- id={} name={} type={} bytes={} chunks={}\n",
                attachment.id,
                attachment.name,
                media,
                attachment.text.len(),
                self.chunk_count(&attachment.id)
            ));
        }
        message
    }

    pub fn embedding_inputs(&self) -> Vec<String> {
        self.chunks
            .iter()
            .map(|chunk| {
                format!(
                    "attachment_id={} chunk={}\n{}",
                    chunk.attachment_id, chunk.chunk_index, chunk.text
                )
            })
            .collect()
    }

    pub fn read(&self, attachment_id: &str, chunk_index: Option<usize>) -> Result<String, String> {
        validate_id(attachment_id)?;
        if let Some(chunk_index) = chunk_index {
            let Some(chunk) = self.chunks.iter().find(|chunk| {
                chunk.attachment_id == attachment_id && chunk.chunk_index == chunk_index
            }) else {
                return Err("Attachment-Chunk nicht gefunden".into());
            };
            return Ok(chunk.text.clone());
        }
        self.attachments
            .iter()
            .find(|attachment| attachment.id == attachment_id)
            .map(|attachment| attachment.text.clone())
            .ok_or_else(|| "Attachment nicht gefunden".into())
    }

    fn chunk_count(&self, attachment_id: &str) -> usize {
        self.chunks
            .iter()
            .filter(|chunk| chunk.attachment_id == attachment_id)
            .count()
    }
}

pub fn read_attachment_tool_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_attachment",
            "description": "Liest ein explizit vom Nutzer angehaengtes Text-Attachment aus dem temporaeren Request-Index.",
            "parameters": {
                "type": "object",
                "properties": {
                    "attachment_id": {"type": "string"},
                    "chunk_index": {"type": "integer", "minimum": 0}
                },
                "required": ["attachment_id"],
                "additionalProperties": false
            }
        }
    })
}

pub fn attach_to_worker_chat_body(body: &mut Value, index: &AttachmentIndex) -> Result<(), String> {
    if index.is_empty() {
        return Ok(());
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| "Worker-Chat-Body ist kein JSON-Objekt".to_string())?;
    let messages = object
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "Worker-Chat-Body enthaelt kein messages-Array".to_string())?;
    messages.insert(
        0,
        serde_json::json!({"role": "system", "content": index.system_message()}),
    );
    object.insert(
        "tools".into(),
        Value::Array(vec![read_attachment_tool_definition()]),
    );
    object
        .entry("tool_choice")
        .or_insert_with(|| Value::String("auto".into()));
    Ok(())
}

/// Hängt alle erlaubten Tool-Definitionen (Attachment-Leser plus Coding-Werk-
/// zeuge) an den Worker-Chat-Body an.
pub fn attach_allowed_tools_to_worker_chat_body(body: &mut Value) -> Result<(), String> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| "Worker-Chat-Body ist kein JSON-Objekt".to_string())?;
    let mut definitions = vec![read_attachment_tool_definition()];
    definitions.extend(crate::coding_tools::tool_definitions());
    object.insert("tools".into(), Value::Array(definitions));
    object
        .entry("tool_choice")
        .or_insert_with(|| Value::String("auto".into()));
    Ok(())
}

pub fn extract_read_attachment_results(
    response: &Value,
    index: &AttachmentIndex,
) -> Result<Vec<ToolResult>, String> {
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
        let call_id = required_string(object, "id")?;
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| "tool_call.function fehlt".to_string())?;
        let name = required_string(function, "name")?;
        if name != "read_attachment" {
            return Err(format!("Tool '{name}' ist nicht freigegeben"));
        }
        let arguments = parse_tool_arguments(function.get("arguments"))?;
        let attachment_id = required_string(&arguments, "attachment_id")
            .or_else(|_| required_string(&arguments, "id"))?;
        let chunk_index = arguments
            .get("chunk_index")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        results.push(ToolResult {
            call_id,
            content: index.read(&attachment_id, chunk_index)?,
        });
    }
    Ok(results)
}

pub fn append_tool_results(
    body: &mut Value,
    assistant_message: Value,
    results: &[ToolResult],
) -> Result<(), String> {
    let messages = body
        .as_object_mut()
        .and_then(|object| object.get_mut("messages"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "Worker-Chat-Body enthaelt kein messages-Array".to_string())?;
    messages.push(assistant_message);
    for result in results {
        messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": result.call_id,
            "name": "read_attachment",
            "content": result.content
        }));
    }
    Ok(())
}

pub fn assistant_message(response: &Value) -> Option<Value> {
    response.pointer("/choices/0/message").cloned()
}

/// Erlaubte Tool-Namen im Chat-Request. Neben dem Attachment-Leser sind das
/// die Coding-Werkzeuge für den Coding-Tab (`coding_tools.rs`). Eine Liste
/// beliebiger Namen wird hier bewusst nicht akzeptiert — nur dieses feste
/// Set.
pub fn is_allowed_tool_name(name: &str) -> bool {
    const CODING_TOOLS: &[&str] = &[
        "run_code",
        "read_file",
        "write_file",
        "list_files",
        "search_files",
        "patch_file",
        "git_status",
    ];
    name == "read_attachment" || CODING_TOOLS.contains(&name)
}

pub fn validate_requested_tools(raw: &Value) -> Result<(), String> {
    let Some(tools) = raw.get("tools") else {
        return Ok(());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| "tools muss ein Array sein".to_string())?;
    for tool in tools {
        let name = tool
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or_else(|| "jedes Tool braucht function.name".to_string())?;
        if !is_allowed_tool_name(name) {
            return Err(format!("Tool '{name}' ist nicht freigegeben"));
        }
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
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

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.as_bytes().contains(&0)
    {
        return Err("Attachment-Name muss ein einfacher Dateiname sein".into());
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err("Attachment-ID enthaelt ungueltige Zeichen".into());
    }
    Ok(())
}

fn validate_type(name: &str, media_type: Option<&str>) -> Result<(), String> {
    let ext_ok = name
        .rsplit_once('.')
        .map(|(_, ext)| ALLOWED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    let media_ok = media_type
        .map(|media| ALLOWED_MEDIA_TYPES.contains(&media))
        .unwrap_or(false);
    if ext_ok || media_ok {
        Ok(())
    } else {
        Err("nur Text-, Code-, Markdown-, JSON- und CSV-Anhaenge sind erlaubt".into())
    }
}

fn validate_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("Attachment-Text darf nicht leer sein".into());
    }
    if text.as_bytes().contains(&0) || text.contains('\u{fffd}') {
        return Err("Binaere Attachments sind nicht erlaubt".into());
    }
    Ok(())
}

fn chunk_attachment(attachment: &Attachment) -> Vec<AttachmentChunk> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < attachment.text.len() {
        let mut end = (start + CHUNK_BYTES).min(attachment.text.len());
        while !attachment.text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(AttachmentChunk {
            attachment_id: attachment.id.clone(),
            chunk_index: chunks.len(),
            text: attachment.text[start..end].to_owned(),
        });
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_explicit_text_attachments_and_builds_index() {
        let raw = serde_json::json!({
            "attachments": [
                {"id": "notes", "name": "notes.md", "content": "alpha"},
                {"name": "data.csv", "media_type": "text/csv", "content": "a,b\n1,2"}
            ]
        });
        let index = AttachmentIndex::from_openai_request(&raw).unwrap().unwrap();
        assert_eq!(index.read("notes", None).unwrap(), "alpha");
        assert_eq!(index.embedding_inputs().len(), 2);
    }

    #[test]
    fn rejects_paths_binary_content_and_unsupported_types() {
        let path = serde_json::json!({
            "attachments": [{"name": "../secret.md", "content": "x"}]
        });
        let binary = serde_json::json!({
            "attachments": [{"name": "x.txt", "content": "a\u{0000}b"}]
        });
        let image = serde_json::json!({
            "attachments": [{"name": "x.png", "content": "text"}]
        });
        assert!(AttachmentIndex::from_openai_request(&path).is_err());
        assert!(AttachmentIndex::from_openai_request(&binary).is_err());
        assert!(AttachmentIndex::from_openai_request(&image).is_err());
    }

    #[test]
    fn only_read_attachment_tool_is_allowed() {
        let ok = serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "read_attachment"}}]
        });
        let bad = serde_json::json!({
            "tools": [{"type": "function", "function": {"name": "shell"}}]
        });
        assert!(validate_requested_tools(&ok).is_ok());
        assert!(validate_requested_tools(&bad).is_err());
    }

    #[test]
    fn extracts_tool_results_from_worker_call() {
        let raw = serde_json::json!({
            "attachments": [{"id": "notes", "name": "notes.txt", "content": "secret text"}]
        });
        let index = AttachmentIndex::from_openai_request(&raw).unwrap().unwrap();
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "read_attachment",
                            "arguments": "{\"attachment_id\":\"notes\"}"
                        }
                    }]
                }
            }]
        });
        let results = extract_read_attachment_results(&response, &index).unwrap();
        assert_eq!(results[0].call_id, "call_1");
        assert_eq!(results[0].content, "secret text");
    }
}
