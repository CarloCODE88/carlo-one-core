#![allow(clippy::all)]
use std::{fs::File, io::Write, path::PathBuf};
use tempfile::TempDir;
use tri_ai_engine::chunk::{
    classify_tensor, pack_gguf, ChunkLoader, LoaderConfig, PackerConfig, TensorCategory,
    WarmupConfig, WarmupStrategy,
};

const NAMES: &[&str] = &[
    "blk.0.attn_q.weight",
    "token_embd.weight",
    "blk.0.ffn_gate.0",
    "tokenizer.ggml.model",
    "blk.0.ffn_gate.weight",
    "blk.0.ffn_gate_inp.weight",
    "blk.1.attn_norm.weight",
    "blk.0.attn_norm.weight",
];
fn u32(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_le_bytes())
}
fn u64(v: &mut Vec<u8>, n: u64) {
    v.extend(n.to_le_bytes())
}
fn text(v: &mut Vec<u8>, s: &str) {
    u64(v, s.len() as u64);
    v.extend(s.as_bytes())
}
fn archive() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("m.gguf");
    let out = temp.path().join("out");
    let mut b = b"GGUF".to_vec();
    u32(&mut b, 3);
    u64(&mut b, NAMES.len() as u64);
    u64(&mut b, 0);
    for (i, n) in NAMES.iter().enumerate() {
        text(&mut b, n);
        u32(&mut b, 1);
        u64(&mut b, 64);
        u32(&mut b, 0);
        u64(&mut b, (i * 2048) as u64)
    }
    while b.len() % 32 != 0 {
        b.push(0)
    }
    for i in 0..NAMES.len() {
        b.extend(std::iter::repeat((i + 1) as u8).take(2048))
    }
    File::create(&source).unwrap().write_all(&b).unwrap();
    pack_gguf(
        &source,
        &out,
        &PackerConfig {
            target_chunk_bytes: 2048,
            zstd_level: 1,
            separate_categories: vec![],
            compress_only_compressible: true,
        },
    )
    .unwrap();
    (temp, out)
}
#[test]
fn fixture_is_intentionally_mixed() {
    assert_eq!(classify_tensor(NAMES[3]), TensorCategory::Tokenizer);
    assert_eq!(classify_tensor(NAMES[5]), TensorCategory::MoeRouter);
}
#[test]
fn warmup_orders_eager_chunks_by_priority() {
    let (_t, dir) = archive();
    let report = WarmupStrategy::new(WarmupConfig::default())
        .execute(&ChunkLoader::open(dir, LoaderConfig::default()).unwrap())
        .unwrap();
    let priorities: Vec<_> = report
        .per_chunk
        .iter()
        .map(|m| {
            m.categories
                .iter()
                .map(|c| c.warmup_priority())
                .min()
                .unwrap()
        })
        .collect();
    assert!(priorities.windows(2).all(|p| p[0] <= p[1]));
    assert!(report.per_chunk[0]
        .categories
        .contains(&TensorCategory::Tokenizer));
}
#[test]
fn warmup_loads_eager_only_and_honors_limit() {
    let (_t, dir) = archive();
    let loader = ChunkLoader::open(&dir, LoaderConfig::default()).unwrap();
    let report = WarmupStrategy::new(WarmupConfig { max_chunks: 2, strict_order: true, critical_only: false })
        .execute(&loader)
        .unwrap();
    assert_eq!(report.chunks_loaded, 2);
    assert!(report
        .per_chunk
        .iter()
        .flat_map(|m| m.categories.iter())
        .all(|c| c.is_eager()));
    assert!(report.per_chunk[0]
        .categories
        .contains(&TensorCategory::Tokenizer));
}
