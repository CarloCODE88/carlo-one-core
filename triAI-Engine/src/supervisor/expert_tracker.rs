#![allow(unused_imports)]
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize)]
pub struct ExpertEvent {
    pub layer_id: u32,
    pub expert_ids: Vec<u32>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SocketMessage {
    event: String,
    data: ExpertData,
}

#[derive(Debug, Clone, Deserialize)]
struct ExpertData {
    layer: u32,
    experts: Vec<u32>,
    #[serde(rename = "timestamp")]
    timestamp_ms: u64,
}

pub struct ExpertTracker {
    socket_path: PathBuf,
    events: Arc<Mutex<Vec<ExpertEvent>>>,
}

impl ExpertTracker {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub fn poll_events(&self) -> Result<Vec<ExpertEvent>, String> {
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| format!("socket connect: {}", e))?;
        let reader = BufReader::new(stream);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line.map_err(|e| format!("read line: {}", e))?;
            if line.trim().is_empty() { continue; }
            let msg: SocketMessage = serde_json::from_str(&line)
                .map_err(|e| format!("parse json: {}", e))?;
            if msg.event != "llama_export_expert_decisions" { continue; }
            let ev = ExpertEvent {
                layer_id: msg.data.layer,
                expert_ids: msg.data.experts,
                timestamp_ms: msg.data.timestamp_ms,
            };
            events.push(ev.clone());
        }
        let mut stored = self.events.lock().map_err(|_| "events poisoned".to_string())?;
        stored.extend(events.clone());
        drop(stored);
        Ok(events)
    }

    pub fn latest_events(&self) -> Vec<ExpertEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn event_count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn tracker_connects_and_parses_events() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let msg = r#"{"event":"llama_export_expert_decisions","data":{"layer":0,"experts":[1,2],"timestamp":1000}}"#;
            let _ = stream.write_all(msg.as_bytes());
        });

        thread::sleep(Duration::from_millis(50));
        let tracker = ExpertTracker::new(&sock_path);
        let events = tracker.poll_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].layer_id, 0);
        assert_eq!(events[0].expert_ids, vec![1, 2]);
    }
}
