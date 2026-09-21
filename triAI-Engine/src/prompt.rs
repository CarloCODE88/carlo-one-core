//! Deterministic, fail-closed prompt preparation before inference.

#[path = "prompt/caps.rs"]
pub mod caps;
#[path = "prompt/compress.rs"]
pub mod compress;
#[path = "prompt/estimate.rs"]
pub mod estimate;
#[path = "prompt/hooks.rs"]
pub mod hooks;

pub use caps::{enforce, PromptCaps, TASK_CAP};
pub use hooks::{prepare, PreparedPrompt, PromptError};
