//! Tensor-name classification for later chunk and warmup policy.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TensorCategory {
    Metadata,
    Tokenizer,
    Embeddings,
    Output,
    Norms,
    Attention,
    DenseFfn,
    MoeRouter,
    MoeExpert,
    KvRuntime,
}

impl TensorCategory {
    pub fn warmup_priority(self) -> u8 {
        match self {
            Self::Metadata => 1,
            Self::Tokenizer => 2,
            Self::Norms => 3,
            Self::MoeRouter => 4,
            Self::Embeddings => 5,
            Self::Output => 6,
            Self::Attention => 7,
            Self::DenseFfn => 8,
            Self::MoeExpert => 9,
            Self::KvRuntime => 10,
        }
    }

    pub fn is_eager(self) -> bool {
        matches!(
            self,
            Self::Metadata | Self::Tokenizer | Self::Norms | Self::MoeRouter
        )
    }
}

pub fn classify_tensor(name: &str) -> TensorCategory {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("tokenizer.") {
        return TensorCategory::Tokenizer;
    }
    if lower.contains("token_embd") || lower.contains("embed_tokens") {
        return TensorCategory::Embeddings;
    }
    if lower == "output" || lower == "output.weight" || lower.contains("lm_head") {
        return TensorCategory::Output;
    }
    if lower.contains("norm") || lower.ends_with(".bias") {
        return TensorCategory::Norms;
    }
    if lower.contains("ffn_gate_inp") || lower.contains(".router") {
        return TensorCategory::MoeRouter;
    }
    if is_moe_expert_name(&lower) {
        return TensorCategory::MoeExpert;
    }
    if ["attn_q", "attn_k", "attn_v", "attn_output"]
        .iter()
        .any(|part| lower.contains(part))
    {
        return TensorCategory::Attention;
    }
    if ["ffn_gate", "ffn_up", "ffn_down"]
        .iter()
        .any(|part| lower.contains(part))
    {
        return TensorCategory::DenseFfn;
    }
    TensorCategory::Metadata
}

fn is_moe_expert_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('.').collect();
    parts.len() >= 4
        && matches!(
            parts.get(parts.len().saturating_sub(2)),
            Some(&"ffn_gate" | &"ffn_up" | &"ffn_down")
        )
        && parts.last().is_some_and(|part| part.parse::<u32>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_representative_tensor_names() {
        assert_eq!(
            classify_tensor("token_embd.weight"),
            TensorCategory::Embeddings
        );
        assert_eq!(
            classify_tensor("blk.0.attn_q.weight"),
            TensorCategory::Attention
        );
        assert_eq!(
            classify_tensor("blk.0.ffn_gate.5"),
            TensorCategory::MoeExpert
        );
        assert_eq!(
            classify_tensor("blk.0.ffn_gate_inp.weight"),
            TensorCategory::MoeRouter
        );
        assert_eq!(classify_tensor("output_norm.weight"), TensorCategory::Norms);
    }

    #[test]
    fn eager_categories_precede_lazy_categories() {
        assert!(TensorCategory::Tokenizer.is_eager());
        assert!(!TensorCategory::MoeExpert.is_eager());
        assert!(
            TensorCategory::Norms.warmup_priority() < TensorCategory::Attention.warmup_priority()
        );
    }
}
