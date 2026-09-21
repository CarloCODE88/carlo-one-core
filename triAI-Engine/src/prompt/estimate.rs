//! Conservative token estimator with an explicit CJK path.

pub fn estimate_tokens(input: &str) -> u32 {
    if input.is_empty() {
        return 0;
    }
    let (cjk, other) = input.chars().fold((0_u32, 0_u32), |(cjk, other), ch| {
        if matches!(ch as u32, 0x3040..=0x30ff | 0x3400..=0x9fff | 0xac00..=0xd7af) {
            (cjk + 1, other)
        } else {
            (cjk, other + 1)
        }
    });
    cjk.saturating_add((other + 3) / 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn estimates_ascii_and_cjk_conservatively() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("你好世界"), 4);
    }
    #[test]
    fn empty_prompt_has_no_estimated_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }
    #[test]
    fn mixed_text_counts_cjk_per_character() {
        assert_eq!(estimate_tokens("你好abcd"), 3);
    }
}
