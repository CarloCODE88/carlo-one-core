// tests/integration.rs
#![allow(clippy::all)]
//! Integration test for dynamic ctx_size propagation from planner to worker.
//!
//! This test spins up a temporary instance of the triAI‑Engine HTTP server,
//! requests a planning step for a known model, starts the model using the
//! returned plan, and then verifies that the worker process receives the
//! exact `ctx_size` that the planner calculated.
//!
//! The assertions are dynamic – the test does not hard‑code any particular
//! token count. It simply checks that the logged `ctx_size` equals the
//! `ctx_size` field returned in the planner response.

use reqwest::blocking::Client;
use serde_json::Value;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

struct EngineGuard(Child);

impl Drop for EngineGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_engine() -> EngineGuard {
    let stub_path = std::env::temp_dir().join("stub-llama-server.py");
    let script = r#"#!/usr/bin/env python3
import sys, socket, time

port = 8901
for i, arg in enumerate(sys.argv):
    if arg == "--port" and i + 1 < len(sys.argv):
        port = int(sys.argv[i + 1])

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(5)

time.sleep(30)
"#;
    let _ = std::fs::write(&stub_path, script);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&stub_path, std::fs::Permissions::from_mode(0o755));
    }

    // Start the engine in the background on an unused port.
    EngineGuard(
        Command::new("./target/release/tri-ai-engine")
            .arg("--config")
            .arg("config/example.toml")
            .env("TRI_AI_LLAMA_SERVER", &stub_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to start tri‑ai‑engine binary"),
    )
}

fn wait_for_health(port: u16) {
    let client = Client::new();
    let url = format!("http://127.0.0.1:{}/health", port);
    for _ in 0..20 {
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn test_dynamic_ctx_size_propagation() {
    // 1. Launch engine daemon.
    let _engine = spawn_engine();
    let port = 8900; // matches example.toml
    wait_for_health(port);

    let client = Client::new();
    let model_id = "sha256:1de498fe269116d448a52cba3796bbad0a2ac4dc1619ff6b46674ba344dcf69d";
    let mut plan_ctx_size = 0;
    for attempt in 0..5 {
        let plan_req = client
            .post(&format!("http://127.0.0.1:{}/api/engine/plan/auto", port))
            .json(&serde_json::json!({
                "model": model_id,
            }))
            .send()
            .expect("Planner request failed");
        let status = plan_req.status();
        let text = plan_req
            .text()
            .expect("Failed to read planner response text");
        assert!(status.is_success(), "Planner returned error: {text}");
        let plan_json: Value = serde_json::from_str(&text).expect("Planner response not JSON");
        let plan_id = plan_json["plan_id"].as_str().expect("Missing plan_id");
        plan_ctx_size = plan_json["ctx_size"].as_u64().expect("Missing ctx_size");

        let start_req = client
            .post(&format!("http://127.0.0.1:{}/api/engine/start", port))
            .json(&serde_json::json!({
                "model": model_id,
                "plan_id": plan_id,
            }))
            .send()
            .expect("Start request failed");
        let start_status = start_req.status();
        let start_text = start_req
            .text()
            .expect("Failed to read start response text");
        if start_status.is_success() {
            break;
        }
        if start_text.contains("stale_plan") && attempt < 4 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        panic!("Start returned error: {start_text}");
    }

    // 4. Give the worker a moment to write its startup log.
    thread::sleep(Duration::from_secs(2));

    // 5. Read the most recent worker log file.
    let log_path = std::path::Path::new("./tri-ai-events.jsonl");
    let log_content = std::fs::read_to_string(log_path).expect("Failed to read engine log");
    let logged_ctx = log_content
        .lines()
        .rev()
        .find(|l| l.contains("ctx_size"))
        .expect("No ctx_size line in log");
    let logged_json: Value = serde_json::from_str(logged_ctx).expect("Log line not valid JSON");
    let logged_val: u64 = logged_json["fields"]["ctx_size"]
        .as_u64()
        .or_else(|| logged_json["ctx_size"].as_u64())
        .expect("Failed to parse ctx_size from log");

    assert_eq!(
        logged_val, plan_ctx_size,
        "Worker ctx_size did not match planner ctx_size"
    );
}
