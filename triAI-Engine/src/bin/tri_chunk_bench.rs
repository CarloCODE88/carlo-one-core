//! Measure cold/warm chunk loading and eager warmup for a packed archive.
use std::{env, process, time::Instant};
use tri_ai_engine::chunk::{ChunkLoader, LoaderConfig, WarmupConfig, WarmupStrategy};

fn main() {
    let Some(dir) = env::args().nth(1) else {
        eprintln!("usage: tri-chunk-bench <archive-dir>");
        process::exit(2);
    };
    let loader = ChunkLoader::open(&dir, LoaderConfig::default()).unwrap_or_else(|e| {
        eprintln!("open failed: {e}");
        process::exit(1);
    });
    let ids: Vec<String> = loader
        .manifest()
        .chunks
        .iter()
        .take(3)
        .map(|c| c.chunk_id.clone())
        .collect();
    if ids.len() < 3 {
        eprintln!("archive needs at least three chunks");
        process::exit(1);
    }
    let mut cold = Vec::new();
    for id in &ids {
        let fresh = ChunkLoader::open(&dir, LoaderConfig::default()).unwrap();
        let start = Instant::now();
        fresh.load_chunk(id).unwrap();
        cold.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mut warm = Vec::new();
    for id in &ids {
        loader.load_chunk(id).unwrap();
        let start = Instant::now();
        loader.load_chunk(id).unwrap();
        warm.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let warmup = WarmupStrategy::new(WarmupConfig::default())
        .execute(&loader)
        .unwrap();
    let disk = loader.manifest().disk_savings_percent();
    println!("{{\"chunk_load_cold_ms\":{:?},\"chunk_load_warm_ms\":{:?},\"warmup_seconds\":[{}],\"disk_savings_percent\":[{}]}}", cold, warm, warmup.total_duration.as_secs_f64(), disk);
}
