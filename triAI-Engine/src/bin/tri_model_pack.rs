use std::{env, process};
use tri_ai_engine::chunk::{pack_gguf, PackerConfig};
fn main() {
    let a: Vec<_> = env::args().collect();
    if a.len() != 3 {
        eprintln!("Usage: {} <input.gguf> <output-dir>", a[0]);
        process::exit(2)
    }
    match pack_gguf(&a[1], &a[2], &PackerConfig::default()) {
        Ok(r) => println!(
            "packed {} tensors into {} chunks ({:.1}% savings)",
            r.tensor_index.tensors.len(),
            r.manifest.chunks.len(),
            r.disk_savings_percent
        ),
        Err(e) => {
            eprintln!("pack failed: {e}");
            process::exit(1)
        }
    }
}
