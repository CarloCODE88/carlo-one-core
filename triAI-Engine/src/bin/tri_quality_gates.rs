//! CLI for validating recorded, three-run benchmark evidence.
use std::{env, process};
use tri_ai_engine::benchmark::{BenchmarkResults, QualityGates};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: tri-quality-gates <benchmark-results.json>");
        process::exit(2);
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            process::exit(2);
        }
    };
    let results: BenchmarkResults = match serde_json::from_str(&content) {
        Ok(results) => results,
        Err(error) => {
            eprintln!("invalid benchmark JSON: {error}");
            process::exit(2);
        }
    };
    match QualityGates::default().validate(&results) {
        Ok(()) => println!("quality gates: PASS"),
        Err(error) => {
            eprintln!("quality gates: FAIL: {}", error.0);
            process::exit(1);
        }
    }
}
