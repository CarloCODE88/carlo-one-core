use super::{
    caps::{enforce, PromptCaps},
    compress::compress,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPrompt {
    pub text: String,
    pub estimated_tokens: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    SecretDetected,
    CapExceeded,
}

pub fn prepare(input: &str, caps: PromptCaps) -> Result<PreparedPrompt, PromptError> {
    if contains_secret(input) {
        return Err(PromptError::SecretDetected);
    }
    let text = compress(input);
    let estimated_tokens = enforce(&text, caps).map_err(|_| PromptError::CapExceeded)?;
    Ok(PreparedPrompt {
        text,
        estimated_tokens,
    })
}

fn contains_secret(input: &str) -> bool {
    input.contains("BEGIN PRIVATE KEY")
        || input
            .split_whitespace()
            .any(|item| item.starts_with("sk-") || item.starts_with("AKIA"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hooks_are_deterministic_and_reject_secrets() {
        assert_eq!(
            prepare("please please explain", PromptCaps::default()),
            prepare("please please explain", PromptCaps::default())
        );
        assert_eq!(
            prepare("key sk-secret", PromptCaps::default()),
            Err(PromptError::SecretDetected)
        );
    }
    #[test]
    fn rejects_cloud_and_private_key_markers() {
        assert_eq!(
            prepare("AKIA012345", PromptCaps::default()),
            Err(PromptError::SecretDetected)
        );
        assert_eq!(
            prepare("BEGIN PRIVATE KEY", PromptCaps::default()),
            Err(PromptError::SecretDetected)
        );
    }
}
