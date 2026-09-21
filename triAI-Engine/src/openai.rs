//! Kleine OpenAI-kompatible Kantenlogik fuer den lokalen llama.cpp-Worker.
//!
//! Dieses Modul validiert nur die Felder, die der Runner selbst braucht, und
//! reicht erlaubte OpenAI-Parameter als JSON weiter. Dadurch bleiben Tool-Calls
//! und Usage-Daten erhalten, ohne dass der Runner jedes Subschema besitzen muss.

use serde_json::{Map, Value};

const CHAT_PASSTHROUGH_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "max_tokens",
    "stop",
    "tools",
    "tool_choice",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "response_format",
    "n",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "stream_options",
];

const EMBEDDINGS_PASSTHROUGH_FIELDS: &[&str] =
    &["encoding_format", "dimensions", "user", "truncate"];

/// Hard per-request output limit. It is enforced server-side before a worker
/// is contacted, independent of client-supplied context or model settings.
pub const MAX_COMPLETION_TOKENS: u64 = 1_200;

pub fn chat_worker_body(raw: &Value) -> Result<(String, bool, Value), String> {
    let object = raw
        .as_object()
        .ok_or_else(|| "Chat-Request muss ein JSON-Objekt sein".to_string())?;
    let model = required_string(object, "model")?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages muss ein nicht leeres Array sein".to_string())?;
    if messages.is_empty() {
        return Err("messages muss ein nicht leeres Array sein".into());
    }
    if object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .is_some_and(|value| value > MAX_COMPLETION_TOKENS)
    {
        return Err(format!(
            "max_tokens darf {} nicht überschreiten",
            MAX_COMPLETION_TOKENS
        ));
    }
    for message in messages {
        validate_chat_message(message)?;
    }
    crate::attachments::validate_requested_tools(raw)?;
    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut prepared_messages = messages.clone();
    prepare_text_messages(&mut prepared_messages)?;
    let mut worker = Map::new();
    worker.insert("model".into(), Value::String(model.clone()));
    worker.insert("messages".into(), Value::Array(prepared_messages));
    worker.insert("stream".into(), Value::Bool(stream));
    copy_present(object, &mut worker, CHAT_PASSTHROUGH_FIELDS);
    Ok((model, stream, Value::Object(worker)))
}

fn prepare_text_messages(messages: &mut [Value]) -> Result<(), String> {
    for message in messages {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        let Some(text) = content.as_str() else {
            continue;
        };
        let prepared = crate::prompt::prepare(text, crate::prompt::PromptCaps::default())
            .map_err(|error| format!("Prompt vor Inferenz abgewiesen: {error:?}"))?;
        *content = Value::String(prepared.text);
    }
    Ok(())
}

pub fn legacy_chat_worker_body(model: &str, prompt: &str) -> Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false
    })
}

pub fn normalize_chat_response(mut worker: Value, requested_model: &str) -> Result<Value, String> {
    let object = worker
        .as_object_mut()
        .ok_or_else(|| "Worker lieferte kein JSON-Objekt".to_string())?;
    if !matches!(object.get("choices"), Some(Value::Array(_))) {
        return Err("Worker-Antwort enthaelt kein choices-Array".into());
    }
    object
        .entry("object")
        .or_insert_with(|| Value::String("chat.completion".into()));
    object
        .entry("model")
        .or_insert_with(|| Value::String(requested_model.into()));
    Ok(worker)
}

/// Token-Nutzung einer einzelnen Worker-Antwort, aufsummierbar über mehrere
/// Runden eines Tool-Loops hinweg (siehe [`Usage::add`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

/// Liest `usage` aus einer Worker-Antwort, falls vorhanden und vollständig.
/// Ein fehlendes oder unvollständiges `usage`-Feld ist kein Fehler — nicht
/// jeder Worker liefert es bei jeder Antwort (z.B. leerer Tool-Ergebnis-Turn).
pub fn usage_from(worker: &Value) -> Option<Usage> {
    let usage = worker.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()?;
    let completion_tokens = usage.get("completion_tokens")?.as_u64()?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

/// Summiert `usage` über mehrere Runden eines Tool-Loops. Jede Runde ist ein
/// eigener Worker-Request (Prompt inklusive der bis dahin akkumulierten
/// Tool-Ergebnisse) — die Tokens der Zwischenrunden gingen bisher verloren,
/// wenn nur die *letzte* Antwort an den Client zurückging.
pub fn accumulate_usage(total: &mut Option<Usage>, worker: &Value) {
    if let Some(round_usage) = usage_from(worker) {
        match total {
            Some(total) => total.add(round_usage),
            None => *total = Some(round_usage),
        }
    }
}

/// Ersetzt `usage` in einer bereits normalisierten Antwort durch die über den
/// gesamten Tool-Loop aufsummierten Werte. Ohne akkumulierte Daten (kein
/// Worker dieser Anfrage lieferte `usage`) bleibt die Antwort unverändert,
/// statt ein irreführendes `usage: null` einzufügen.
pub fn apply_usage(response: &mut Value, total: Option<Usage>) {
    let Some(total) = total else { return };
    if let Some(object) = response.as_object_mut() {
        object.insert(
            "usage".into(),
            serde_json::json!({
                "prompt_tokens": total.prompt_tokens,
                "completion_tokens": total.completion_tokens,
                "total_tokens": total.total_tokens,
            }),
        );
    }
}

pub fn chat_text(worker: &Value) -> Result<String, String> {
    worker
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Worker-Antwort enthaelt keinen choices[0].message.content-Text".into())
}

pub fn embeddings_worker_body(raw: &Value) -> Result<(String, Value), String> {
    let object = raw
        .as_object()
        .ok_or_else(|| "Embeddings-Request muss ein JSON-Objekt sein".to_string())?;
    let model = required_string(object, "model")?;
    let input = object
        .get("input")
        .ok_or_else(|| "input fehlt".to_string())?
        .clone();
    validate_embedding_input(&input)?;
    let mut worker = Map::new();
    worker.insert("model".into(), Value::String(model.clone()));
    worker.insert("input".into(), input);
    copy_present(object, &mut worker, EMBEDDINGS_PASSTHROUGH_FIELDS);
    Ok((model, Value::Object(worker)))
}

pub fn normalize_embeddings_response(
    mut worker: Value,
    requested_model: &str,
) -> Result<Value, String> {
    let object = worker
        .as_object_mut()
        .ok_or_else(|| "Worker lieferte kein JSON-Objekt".to_string())?;
    if !matches!(object.get("data"), Some(Value::Array(_))) {
        return Err("Worker-Antwort enthaelt kein data-Array".into());
    }
    object
        .entry("object")
        .or_insert_with(|| Value::String("list".into()));
    object
        .entry("model")
        .or_insert_with(|| Value::String(requested_model.into()));
    Ok(worker)
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} muss ein nicht leerer String sein"))
}

fn copy_present(source: &Map<String, Value>, target: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = source.get(*key) {
            target.insert((*key).into(), value.clone());
        }
    }
}

fn validate_chat_message(message: &Value) -> Result<(), String> {
    let object = message
        .as_object()
        .ok_or_else(|| "jede Message muss ein JSON-Objekt sein".to_string())?;
    required_string(object, "role")?;
    if object.contains_key("content") || object.contains_key("tool_calls") {
        return Ok(());
    }
    Err("jede Message braucht content oder tool_calls".into())
}

fn validate_embedding_input(input: &Value) -> Result<(), String> {
    match input {
        Value::String(value) if !value.is_empty() => Ok(()),
        Value::Array(values) if !values.is_empty() => {
            for value in values {
                match value {
                    Value::String(text) if !text.is_empty() => {}
                    Value::Array(tokens) if !tokens.is_empty() => {
                        if !tokens.iter().all(|token| token.as_i64().is_some()) {
                            return Err("Token-Arrays duerfen nur ganze Zahlen enthalten".into());
                        }
                    }
                    _ => return Err("input enthaelt ein ungueltiges Element".into()),
                }
            }
            Ok(())
        }
        _ => Err("input muss ein String oder ein nicht leeres Array sein".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_body_preserves_full_history_and_allowed_parameters() {
        let request = serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "stay concise"},
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "tool", "tool_call_id": "call_1", "content": "result"}
            ],
            "temperature": 0.2,
            "top_p": 0.9,
            "max_tokens": 64,
            "tools": [{"type": "function", "function": {"name": "read_attachment"}}],
            "tool_choice": "auto",
            "stream": false,
            "unused": "drop"
        });
        let (model, stream, body) = chat_worker_body(&request).unwrap();
        assert_eq!(model, "m");
        assert!(!stream);
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        assert_eq!(body["temperature"], serde_json::json!(0.2));
        assert_eq!(body["tools"][0]["function"]["name"], "read_attachment");
        assert!(body.get("unused").is_none());
    }

    #[test]
    fn chat_body_applies_prompt_hooks_before_worker_body() {
        let request = serde_json::json!({
            "model": "m", "messages": [{"role":"user", "content":"please explain https://example.invalid Rust"}]
        });
        let (_, _, worker) = chat_worker_body(&request).unwrap();
        assert_eq!(worker["messages"][0]["content"], "explain Rust");
        let secret =
            serde_json::json!({"model":"m", "messages":[{"role":"user", "content":"sk-secret"}]});
        assert!(chat_worker_body(&secret).is_err());
    }

    #[test]
    fn chat_body_rejects_output_token_budget_over_cap() {
        let request = serde_json::json!({
            "model": "m", "max_tokens": MAX_COMPLETION_TOKENS + 1,
            "messages": [{"role":"user", "content":"hi"}]
        });
        assert!(chat_worker_body(&request)
            .unwrap_err()
            .contains("max_tokens"));
    }

    #[test]
    fn chat_body_passes_through_stream_options_for_streamed_usage() {
        // OpenAI-Clients fragen finalen usage-Tokens im SSE-Stream ueber
        // `stream_options.include_usage` an; da der Streaming-Pfad die
        // Worker-SSE-Antwort 1:1 durchreicht (kein eigenes Parsing), muss
        // dieses Feld unveraendert beim Worker ankommen, sonst liefert
        // llama.cpp den Usage-Chunk gar nicht erst.
        let request = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        let (_, stream, body) = chat_worker_body(&request).unwrap();
        assert!(stream);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn chat_response_preserves_usage_and_tool_calls() {
        let worker = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read_attachment", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let normalized = normalize_chat_response(worker, "m").unwrap();
        assert_eq!(normalized["model"], "m");
        assert_eq!(
            normalized["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(normalized["usage"]["total_tokens"], 7);
    }

    #[test]
    fn embeddings_body_accepts_string_and_array_inputs() {
        let one = serde_json::json!({"model": "m", "input": "hello"});
        let many = serde_json::json!({
            "model": "m",
            "input": ["hello", "world"],
            "encoding_format": "float"
        });
        assert!(embeddings_worker_body(&one).is_ok());
        let (_, body) = embeddings_worker_body(&many).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["encoding_format"], "float");
    }

    #[test]
    fn embeddings_response_keeps_worker_data_and_usage() {
        let worker = serde_json::json!({
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
            "usage": {"prompt_tokens": 2, "total_tokens": 2}
        });
        let normalized = normalize_embeddings_response(worker, "m").unwrap();
        assert_eq!(normalized["object"], "list");
        assert_eq!(normalized["model"], "m");
        assert_eq!(
            normalized["data"][0]["embedding"][1],
            serde_json::json!(0.2)
        );
        assert_eq!(normalized["usage"]["total_tokens"], 2);
    }

    #[test]
    fn usage_from_reads_complete_worker_usage() {
        let worker = serde_json::json!({
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        });
        assert_eq!(
            usage_from(&worker),
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                total_tokens: 7
            })
        );
    }

    #[test]
    fn usage_from_derives_total_when_worker_omits_it() {
        let worker = serde_json::json!({
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        assert_eq!(
            usage_from(&worker).unwrap().total_tokens,
            7,
            "total_tokens muss aus prompt+completion abgeleitet werden, wenn der Worker es weglaesst"
        );
    }

    #[test]
    fn usage_from_is_none_without_usage_field() {
        let worker = serde_json::json!({"choices": []});
        assert_eq!(usage_from(&worker), None);
    }

    #[test]
    fn accumulate_usage_sums_across_rounds_and_skips_missing_usage() {
        let mut total = None;
        accumulate_usage(
            &mut total,
            &serde_json::json!({"usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}}),
        );
        // Eine Zwischenrunde ohne usage-Feld darf die bisherige Summe nicht
        // zerstoeren oder verfaelschen.
        accumulate_usage(&mut total, &serde_json::json!({"choices": []}));
        accumulate_usage(
            &mut total,
            &serde_json::json!({"usage": {"prompt_tokens": 15, "completion_tokens": 4, "total_tokens": 19}}),
        );
        assert_eq!(
            total,
            Some(Usage {
                prompt_tokens: 25,
                completion_tokens: 7,
                total_tokens: 32
            })
        );
    }

    #[test]
    fn apply_usage_overwrites_final_response_usage_with_accumulated_total() {
        let mut response = serde_json::json!({
            "choices": [],
            // Ohne Aggregation waere das hier die einzige (unvollstaendige)
            // usage-Angabe, die der Client zu sehen bekaeme.
            "usage": {"prompt_tokens": 15, "completion_tokens": 4, "total_tokens": 19}
        });
        apply_usage(
            &mut response,
            Some(Usage {
                prompt_tokens: 25,
                completion_tokens: 7,
                total_tokens: 32,
            }),
        );
        assert_eq!(response["usage"]["prompt_tokens"], 25);
        assert_eq!(response["usage"]["completion_tokens"], 7);
        assert_eq!(response["usage"]["total_tokens"], 32);
    }

    #[test]
    fn apply_usage_leaves_response_untouched_without_accumulated_data() {
        let mut response = serde_json::json!({"choices": []});
        apply_usage(&mut response, None);
        assert!(response.get("usage").is_none());
    }
}
