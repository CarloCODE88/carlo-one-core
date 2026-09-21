use super::estimate::estimate_tokens;

pub const TASK_CAP: u32 = 1_200;

#[derive(Debug, Clone, Copy)]
pub struct PromptCaps {
    pub task_tokens: u32,
    pub run_tokens: u32,
    pub call_tokens: u32,
}
impl Default for PromptCaps {
    fn default() -> Self {
        Self {
            task_tokens: TASK_CAP,
            run_tokens: TASK_CAP,
            call_tokens: TASK_CAP,
        }
    }
}

pub fn enforce(input: &str, caps: PromptCaps) -> Result<u32, &'static str> {
    let estimate = estimate_tokens(input);
    if caps.task_tokens > TASK_CAP
        || estimate > caps.task_tokens
        || estimate > caps.run_tokens
        || estimate > caps.call_tokens
    {
        return Err("prompt token cap exceeded");
    }
    Ok(estimate)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caps_are_hard_and_checked_before_inference() {
        assert!(enforce(&"a".repeat(4_801), PromptCaps::default()).is_err());
        assert!(enforce(
            "safe",
            PromptCaps {
                task_tokens: 1_201,
                ..PromptCaps::default()
            }
        )
        .is_err());
    }
    #[test]
    fn run_and_call_caps_are_independent() {
        assert!(enforce(
            "abcdefgh",
            PromptCaps {
                task_tokens: TASK_CAP,
                run_tokens: 1,
                call_tokens: TASK_CAP
            }
        )
        .is_err());
        assert!(enforce(
            "abcdefgh",
            PromptCaps {
                task_tokens: TASK_CAP,
                run_tokens: TASK_CAP,
                call_tokens: 1
            }
        )
        .is_err());
    }
}
