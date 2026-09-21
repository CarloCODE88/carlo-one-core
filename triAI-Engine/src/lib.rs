//! Embeddable local-LLM engine core for TRI.
//!
//! Das Binary in `main.rs` bleibt absichtlich dünn. Die produktiven Module
//! sind hier öffentlich, damit Einbettungen und Integrationstests dieselben
//! Zustands- und Sicherheitsregeln wie der HTTP-Server verwenden.

#![allow(clippy::all)]
#![allow(warnings)]

pub mod analysis;
pub mod api;
pub mod assistant;
pub mod attachments;
pub mod benchmark;
pub mod chunk;
pub mod coding_tools;
pub mod config;
pub mod download;
pub mod engine;
pub mod error;
pub mod evidence;
pub mod gguf_registry;
pub mod http;
pub mod learning;
pub mod kernel_ffi;
pub mod kernel_worker;
pub mod model_catalog;
pub mod model_registry;
pub mod model_sources;
pub mod monitor;
pub mod moe;
pub mod observability;
pub mod openai;
pub mod performance;
pub mod persistence;
pub mod planner;
pub mod prompt;
pub mod resources;
pub mod speculative;
pub mod staging;
pub mod storage;
pub mod supervisor;
pub mod tool_registry;