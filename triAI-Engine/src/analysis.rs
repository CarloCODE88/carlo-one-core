//! Background analysis and policy-promotion foundations.
//!
//! This module is deliberately off the inference path.  Its input types carry
//! aggregate counters only: prompts, code, outputs, and secrets are neither
//! accepted nor persisted here.

#[path = "analysis/gates.rs"]
pub mod gates;
#[path = "analysis/job.rs"]
pub mod job;
#[path = "analysis/policy.rs"]
pub mod policy;
#[path = "analysis/store.rs"]
pub mod store;

pub use gates::{PromotionDecision, PromotionGates};
pub use job::{AggregateWindow, AnalysisJob, AnalysisKind, AnalysisScheduler};
pub use policy::{PolicyProposal, PolicyVersioner};
pub use store::MetricPolicyStore;
