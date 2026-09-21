//! Text-only compression. Fenced code remains byte-identical.

pub fn compress(input: &str) -> String {
    let mut result = String::new();
    let mut code = false;
    let mut previous = None;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            code = !code;
            result.push_str(line);
            result.push('\n');
            previous = None;
            continue;
        }
        let normalized = if code {
            line.to_owned()
        } else {
            compress_line(line)
        };
        if !code && normalized.is_empty() {
            continue;
        }
        if !code && previous.as_deref() == Some(normalized.as_str()) {
            continue;
        }
        previous = Some(normalized.clone());
        result.push_str(&normalized);
        result.push('\n');
    }
    result.trim_end().to_owned()
}

fn compress_line(line: &str) -> String {
    let without_urls: Vec<_> = line
        .split_whitespace()
        .filter(|word| !word.starts_with("http://") && !word.starts_with("https://"))
        .collect();
    without_urls
        .into_iter()
        .filter(|word| {
            !matches!(
                word.to_ascii_lowercase().as_str(),
                "bitte" | "please" | "eigentlich" | "basically" | "just" | "halt" | "irgendwie"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_fillers_urls_and_duplicate_text_but_preserves_code() {
        let value = "Bitte erkläre https://example.invalid Rust\nBitte erkläre https://example.invalid Rust\n```rs\nlet x = \"https://keep\";\n```";
        assert_eq!(
            compress(value),
            "erkläre Rust\n```rs\nlet x = \"https://keep\";\n```"
        );
    }
    #[test]
    fn keeps_non_url_words_around_removed_url() {
        assert_eq!(compress("keep https://example.invalid this"), "keep this");
    }
    #[test]
    fn code_fence_content_is_never_normalized() {
        assert_eq!(
            compress("```\nplease https://example.invalid\n```"),
            "```\nplease https://example.invalid\n```"
        );
    }
}
