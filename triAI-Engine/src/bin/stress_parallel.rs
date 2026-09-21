//! Paralleler Lasttest-Harness fuer triAI-Engine.
//! Simuliert N parallele Clients mit variierenden Kontext-Laengen
//! und misst TTFT, TPS, Speicherverbrauch und Stabilitaet.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TaskResult {
    task_id: usize,
    context_size: usize,
    ttft_ms: u128,
    total_ms: u128,
    tokens_generated: u64,
    status: TaskStatus,
}

#[derive(Debug, Clone, PartialEq)]
enum TaskStatus {
    Success,
    Timeout,
    Error(String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct RunStats {
    iteration: usize,
    concurrent_tasks: usize,
    avg_ttft_ms: f64,
    p99_ttft_ms: f64,
    avg_total_ms: f64,
    avg_tps: f64,
    success_rate: f64,
    max_vram_mb: u64,
}

#[derive(Debug, Clone)]
struct StressConfig {
    url: String,
    model: String,
    concurrent_tasks: usize,
    iterations: usize,
    context_sizes: Vec<usize>,
    max_tokens: usize,
    timeout_ms: u64,
}

impl Default for StressConfig {
    fn default() -> Self {
        StressConfig {
            url: "http://127.0.0.1:8765/v1/chat/completions".to_string(),
            model: "INGRIED".to_string(),
            concurrent_tasks: 10,
            iterations: 5,
            context_sizes: vec![512, 2048, 4096, 8192],
            max_tokens: 100,
            timeout_ms: 120_000,
        }
    }
}

fn generate_context_bytes(size: usize) -> String {
    let chunk = "Die schnelle braune Fuchs springt ueber den faulen Hund. ";
    let repeats = (size / chunk.len()).max(1);
    chunk.repeat(repeats)[..size.min(chunk.len() * repeats)].to_string()
}

fn generate_prompt(task_id: usize, context: &str) -> String {
    format!(
        "[Task {}] Analysiere den folgenden Text und gib eine Zusammenfassung:\n\n{}\n\nZusammenfassung:",
        task_id, context
    )
}

async fn run_single_task(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    task_id: usize,
    context_size: usize,
    max_tokens: usize,
    timeout: Duration,
) -> TaskResult {
    let context = generate_context_bytes(context_size);
    let prompt = generate_prompt(task_id, &context);
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": false,
        "temperature": 0.7,
    });

    let req_start = Instant::now();
    let result = match client.post(url).json(&payload).timeout(timeout).send().await {
        Ok(res) => match res.text().await {
            Ok(body) => {
                let total = req_start.elapsed();
                let tokens = body.chars().count() as u64;
                TaskResult {
                    task_id,
                    context_size,
                    ttft_ms: req_start.elapsed().as_millis(),
                    total_ms: total.as_millis(),
                    tokens_generated: tokens,
                    status: TaskStatus::Success,
                }
            }
            Err(e) => TaskResult {
                task_id,
                context_size,
                ttft_ms: 0,
                total_ms: req_start.elapsed().as_millis(),
                tokens_generated: 0,
                status: TaskStatus::Error(e.to_string()),
            },
        },
        Err(e) => TaskResult {
            task_id,
            context_size,
            ttft_ms: 0,
            total_ms: req_start.elapsed().as_millis(),
            tokens_generated: 0,
            status: if e.is_timeout() {
                TaskStatus::Timeout
            } else {
                TaskStatus::Error(e.to_string())
            },
        },
    };
    result
}

fn percentile(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((pct / 100.0) * (sorted.len() - 1) as f64) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn compute_stats(iteration: usize, config: &StressConfig, results: &[TaskResult]) -> RunStats {
    let total = results.len();
    let successes: Vec<&TaskResult> = results.iter().filter(|r| r.status == TaskStatus::Success).collect();
    let success_count = successes.len();
    let success_rate = success_count as f64 / total as f64;

    let ttfts: Vec<f64> = successes.iter().map(|r| r.ttft_ms as f64).collect();
    let totals: Vec<f64> = successes.iter().map(|r| r.total_ms as f64).collect();
    let tps_vals: Vec<f64> = successes
        .iter()
        .map(|r| if r.total_ms > 0 { r.tokens_generated as f64 / (r.total_ms as f64 / 1000.0) } else { 0.0 })
        .collect();

    let avg_ttft = if !ttfts.is_empty() { ttfts.iter().sum::<f64>() / ttfts.len() as f64 } else { 0.0 };
    let p99_ttft = percentile(&ttfts, 99.0);
    let avg_total = if !totals.is_empty() { totals.iter().sum::<f64>() / totals.len() as f64 } else { 0.0 };
    let avg_tps = if !tps_vals.is_empty() { tps_vals.iter().sum::<f64>() / tps_vals.len() as f64 } else { 0.0 };

    let vram = get_vram_usage();

    RunStats {
        iteration,
        concurrent_tasks: config.concurrent_tasks,
        avg_ttft_ms: avg_ttft,
        p99_ttft_ms: p99_ttft,
        avg_total_ms: avg_total,
        avg_tps,
        success_rate,
        max_vram_mb: vram,
    }
}

fn get_vram_usage() -> u64 {
    let output = match std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return 0,
    };
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return stdout.lines().next()
            .and_then(|l| l.trim().parse::<u64>().ok())
            .unwrap_or(0);
    }
    0
}

async fn run_stress_test(config: Arc<StressConfig>) -> Vec<RunStats> {
    let client = Arc::new(reqwest::Client::new());
    let mut all_stats = Vec::new();

    println!("=== triAI-Engine Parallel Stress Test ===");
    println!("URL: {} | Model: {} | Tasks: {} | Iterations: {}",
        config.url, config.model, config.concurrent_tasks, config.iterations);
    println!("Context sizes: {:?}", config.context_sizes);
    println!();

    for iteration in 0..config.iterations {
        println!("--- Iteration {}/{} ---", iteration + 1, config.iterations);
        let start = Instant::now();
        let results: Arc<Mutex<Vec<TaskResult>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for task_id in 0..config.concurrent_tasks {
            let client_clone = client.clone();
            let config_clone = config.clone();
            let results_clone = results.clone();

            let handle = tokio::spawn(async move {
                let ctx_size = config_clone.context_sizes[task_id % config_clone.context_sizes.len()];
                let result = run_single_task(
                    &client_clone,
                    &config_clone.url,
                    &config_clone.model,
                    task_id,
                    ctx_size,
                    config_clone.max_tokens,
                    Duration::from_millis(config_clone.timeout_ms),
                ).await;
                let mut results = results_clone.lock().await;
                results.push(result);
            });
            handles.push(handle);
        }

        // Wait for all tasks to complete
        for handle in handles {
            let _ = handle.await;
        }

        let results_guard = results.lock().await;
        let stats = compute_stats(iteration, &config, &results_guard);
        all_stats.push(stats.clone());

        let elapsed = start.elapsed();
        println!("  Iteration {} done in {:.2}s", iteration + 1, elapsed.as_secs_f64());
        println!("  Avg TTFT: {:.1}ms | P99 TTFT: {:.1}ms | Avg TPS: {:.1} | Success: {:.1}% | VRAM: {}MB",
            stats.avg_ttft_ms, stats.p99_ttft_ms, stats.avg_tps,
            stats.success_rate * 100.0, stats.max_vram_mb);
        println!();

        drop(results_guard);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    all_stats
}

fn analyze_and_report(stats: &[RunStats], config: &StressConfig) {
    println!("=== Stress Test Report ===");
    println!("Configuration: {} concurrent tasks, {} iterations", config.concurrent_tasks, config.iterations);
    println!();

    if stats.is_empty() {
        println!("No data collected.");
        return;
    }

    let avg_ttft: f64 = stats.iter().map(|s| s.avg_ttft_ms).sum::<f64>() / stats.len() as f64;
    let avg_tps: f64 = stats.iter().map(|s| s.avg_tps).sum::<f64>() / stats.len() as f64;
    let avg_success: f64 = stats.iter().map(|s| s.success_rate).sum::<f64>() / stats.len() as f64;
    let avg_vram: f64 = stats.iter().map(|s| s.max_vram_mb as f64).sum::<f64>() / stats.len() as f64;
    let max_p99_ttft: f64 = stats.iter().map(|s| s.p99_ttft_ms).fold(0.0f64, |a, b| a.max(b));

    println!("Summary:");
    println!("  Average TTFT:     {:.1} ms", avg_ttft);
    println!("  Max P99 TTFT:     {:.1} ms", max_p99_ttft);
    println!("  Average TPS:      {:.1} tokens/s", avg_tps);
    println!("  Average Success:  {:.1}%", avg_success * 100.0);
    println!("  Average VRAM:     {:.0} MB", avg_vram);
    println!();

    if let Some(pass) = stats.last() {
        if pass.success_rate >= 0.95 && pass.p99_ttft_ms < 30000.0 {
            println!("RESULT: PASS - System stable under load");
        } else {
            println!("RESULT: WARN - Degradation detected under load");
            println!("  Consider reducing concurrent_tasks or increasing timeout");
        }
    }
}

fn save_optimal_config(stats: &[RunStats]) {
    let best = stats.iter().min_by(|a, b| {
        b.avg_ttft_ms.partial_cmp(&a.avg_ttft_ms).unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(best) = best {
        let optimal = json!({
            "concurrent_tasks": best.concurrent_tasks,
            "avg_ttft_ms": best.avg_ttft_ms,
            "avg_tps": best.avg_tps,
            "success_rate": best.success_rate,
        });
        let _ = std::fs::write("/tmp/optimal_config.json", optimal.to_string());
        println!("Optimal config saved to /tmp/optimal_config.json");
    }
}

#[tokio::main]
async fn main() {
    let config = Arc::new(StressConfig {
        url: std::env::var("TRI_STRESS_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8765/v1/chat/completions".to_string()),
        model: std::env::var("TRI_STRESS_MODEL")
            .unwrap_or_else(|_| "INGRIED".to_string()),
        concurrent_tasks: std::env::var("TRI_STRESS_CONCURRENT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        iterations: std::env::var("TRI_STRESS_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
        context_sizes: vec![512, 2048, 4096, 8192],
        max_tokens: 100,
        timeout_ms: 120_000,
    });

    let stats = run_stress_test(config.clone()).await;
    analyze_and_report(&stats, &config);
    save_optimal_config(&stats);
}
