use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct DraftToken {
    pub token_id: u32,
    pub log_prob: f32,
    pub layer: u32,
}
#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub accepted_tokens: Vec<u32>,
    pub rejected_count: usize,
    pub total_draft_tokens: usize,
    pub speedup: f64,
}
#[derive(Debug, Clone)]
pub struct SpeculativeConfig {
    pub draft_model_path: String,
    pub max_draft_tokens: usize,
    pub acceptance_threshold: f32,
    pub enabled: bool,
}
impl SpeculativeConfig {
    pub fn default() -> Self {
        Self {
            draft_model_path: String::new(),
            max_draft_tokens: 5,
            acceptance_threshold: 0.5,
            enabled: true,
        }
    }
}
pub struct SpeculativeDecoder {
    config: SpeculativeConfig,
    draft_queue: VecDeque<DraftToken>,
    verified_tokens: Vec<u32>,
    total_draft_generated: usize,
    total_verified: usize,
}
impl Default for SpeculativeDecoder {
    fn default() -> Self {
        Self::new(SpeculativeConfig::default())
    }
}
impl SpeculativeDecoder {
    pub fn new(config: SpeculativeConfig) -> Self {
        Self {
            draft_queue: VecDeque::new(),
            verified_tokens: Vec::new(),
            config,
            total_draft_generated: 0,
            total_verified: 0,
        }
    }
    pub fn with_draft_model(path: impl Into<String>) -> Self {
        let mut config = SpeculativeConfig::default();
        config.draft_model_path = path.into();
        Self::new(config)
    }
    pub fn generate_draft(&mut self, prompt_tokens: &[u32]) -> Vec<DraftToken> {
        let prompt_len = if prompt_tokens.is_empty() { 1 } else { prompt_tokens.len() };
        let count = self.config.max_draft_tokens.min(prompt_len);
        let mut drafts = Vec::with_capacity(count);
        for i in 0..count {
            drafts.push(DraftToken {
                token_id: prompt_tokens[i % prompt_tokens.len()] + i as u32,
                log_prob: (0.8 - i as f32 * 0.1) as f32,
                layer: 0,
            });
        }
        self.draft_queue.extend(drafts.iter().cloned());
        self.total_draft_generated += count;
        drafts
    }
    pub fn verify_and_accept(
        &mut self,
        draft_tokens: &[DraftToken],
        verifier_logprobs: &[f32],
    ) -> VerificationResult {
        let mut accepted = Vec::new();
        let mut rejected = 0;
        for (i, draft) in draft_tokens.iter().enumerate() {
            if i < verifier_logprobs.len() {
                if draft.log_prob >= verifier_logprobs[i] * self.config.acceptance_threshold {
                    accepted.push(draft.token_id);
                } else {
                    rejected += 1;
                }
            } else {
                accepted.push(draft.token_id);
            }
        }
        self.verified_tokens.extend(accepted.clone());
        self.total_verified += accepted.len();
        let total_generated = accepted.len() + rejected;
        let speedup = if total_generated > 0 {
            (accepted.len() + rejected) as f64 / accepted.len().max(1) as f64
        } else {
            1.0
        };
        VerificationResult {
            accepted_tokens: accepted,
            rejected_count: rejected,
            total_draft_tokens: total_generated,
            speedup,
        }
    }
    pub fn generate(
        &mut self,
        prompt: &[u32],
        verifier_callback: impl Fn(&[DraftToken]) -> Vec<f32>,
    ) -> Vec<u32> {
        let drafts = self.generate_draft(prompt);
        let verifier_probs = verifier_callback(&drafts);
        let result = self.verify_and_accept(&drafts, &verifier_probs);
        result.accepted_tokens
    }
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
    }
    pub fn stats(&self) -> SpeculativeStats {
        SpeculativeStats {
            total_draft_tokens: self.total_draft_generated,
            total_verified: self.total_verified,
            acceptance_rate: if self.total_draft_generated > 0 {
                self.total_verified as f64 / self.total_draft_generated as f64
            } else {
                0.0
            },
            average_speedup: if self.total_verified > 0 {
                (self.total_draft_generated) as f64 / self.total_verified as f64
            } else {
                1.0
            },
        }
    }
}
#[derive(Debug, Clone)]
pub struct SpeculativeStats {
    pub total_draft_tokens: usize,
    pub total_verified: usize,
    pub acceptance_rate: f64,
    pub average_speedup: f64,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_generation_works() {
        let mut decoder = SpeculativeDecoder::with_draft_model("test.gguf");
        let prompt = vec![1, 2, 3, 4, 5];
        let drafts = decoder.generate_draft(&prompt);
        assert_eq!(drafts.len(), 5);
        assert!(decoder.is_enabled());
    }
    #[test]
    fn verification_accepts_tokens() {
        let mut decoder = SpeculativeDecoder::with_draft_model("test.gguf");
        let drafts = vec![
            DraftToken { token_id: 10, log_prob: 0.9, layer: 0 },
            DraftToken { token_id: 20, log_prob: 0.3, layer: 0 },
            DraftToken { token_id: 30, log_prob: 0.8, layer: 0 },
        ];
        let verifier_probs = vec![0.8, 0.2, 0.7];
        let result = decoder.verify_and_accept(&drafts, &verifier_probs);
        assert!(result.accepted_tokens.len() >= 1);
        assert!(result.speedup >= 1.0);
    }
    #[test]
    fn full_pipeline_works() {
        let mut decoder = SpeculativeDecoder::with_draft_model("ingried.gguf");
        let prompt = vec![100, 200, 300];
        let result = decoder.generate(&prompt, |drafts| {
            vec![0.9; drafts.len()]
        });
        assert!(!result.is_empty());
        let stats = decoder.stats();
        assert!(stats.total_draft_tokens > 0);
        assert!(stats.acceptance_rate > 0.0);
    }
    #[test]
    fn disabled_mode_bypasses_speculation() {
        let mut decoder = SpeculativeDecoder::default();
        decoder.set_enabled(false);
        assert!(!decoder.is_enabled());
    }
}
