//! Checking that the server read the whole prompt. Servers with a small
//! context (ollama's default is 4096 tokens on most machines) can cut the
//! start of an oversized prompt without any error; the only trace is a
//! smaller `usage.prompt_tokens` in the response.

/// Bytes per token used for estimates. Build-log text tokenises at about 2.6
/// bytes per token (measured against ollama), prose at about 4. The larger
/// figure under-estimates the token count, so the estimate is a floor.
pub const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// Warn when the server reports fewer tokens than the floor estimate. Measured
/// against ollama with a 4096 context: untruncated prompts reported 1.17 to
/// 1.52 times the estimate, while truncated ones reported a fixed ~2051 tokens
/// (0.27 to 0.69 times the estimate). A 50% threshold missed the 11 to 17 KB
/// band, where most near-limit prompts fall.
const WARN_BELOW_PERCENT: u64 = 100;

/// What the reported prompt size says about whether the prompt arrived whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCheck {
    /// The server read about as much as was sent.
    Complete,
    /// The server read much less than was sent: the prompt was probably cut.
    Truncated { reported: u32, expected: u32 },
    /// The server did not report its prompt size.
    Unverified,
}

impl PromptCheck {
    /// A sentence for the user, when there is something to say.
    pub fn warning(self) -> Option<String> {
        match self {
            PromptCheck::Complete => None,
            PromptCheck::Truncated { reported, expected } => Some(format!(
                "The server read only {reported} prompt tokens but about {expected} were sent; \
                 the start of the diff was probably dropped. Raise the server's context size \
                 (OLLAMA_CONTEXT_LENGTH for ollama, `llama-server -c` for llama.cpp)."
            )),
            PromptCheck::Unverified => {
                Some("The server did not report the prompt size, so it could not be verified that the whole diff was read.".to_owned())
            }
        }
    }

    /// Whether the verdict should not be relied on.
    pub fn unreliable(self) -> bool {
        matches!(self, PromptCheck::Truncated { .. })
    }
}

/// Compare the prompt that was sent with what the server says it read.
pub fn check_prompt(prompt_bytes: usize, reported: Option<u32>) -> PromptCheck {
    let Some(reported) = reported else {
        return PromptCheck::Unverified;
    };
    let expected = prompt_bytes.div_ceil(BYTES_PER_TOKEN_ESTIMATE) as u64;
    if u64::from(reported) * 100 < expected * WARN_BELOW_PERCENT {
        PromptCheck::Truncated {
            reported,
            expected: u32::try_from(expected).unwrap_or(u32::MAX),
        }
    } else {
        PromptCheck::Complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_read_whole_is_complete() {
        // Measured against ollama with a 4096 context (prompt bytes, tokens).
        for (bytes, reported) in [(4218, 1236), (5983, 2041), (8925, 3383)] {
            assert_eq!(check_prompt(bytes, Some(reported)), PromptCheck::Complete);
        }
    }

    #[test]
    fn the_cut_ollama_reported_is_caught_even_for_near_limit_prompts() {
        // 11.9 KB and 14.9 KB prompts were cut to 2051 tokens.
        for bytes in [11_911, 14_900, 20_878, 29_850] {
            assert!(check_prompt(bytes, Some(2051)).unreliable(), "{bytes}");
        }
    }

    #[test]
    fn a_much_smaller_report_means_truncation() {
        let check = check_prompt(100_000, Some(4096));
        assert_eq!(
            check,
            PromptCheck::Truncated {
                reported: 4096,
                expected: 25_000
            }
        );
        assert!(check.unreliable());
        assert!(check.warning().unwrap().contains("4096"));
    }

    #[test]
    fn no_report_is_unverified_but_not_unreliable() {
        let check = check_prompt(100_000, None);
        assert_eq!(check, PromptCheck::Unverified);
        assert!(!check.unreliable() && check.warning().is_some());
    }

    #[test]
    fn a_complete_prompt_has_no_warning() {
        assert_eq!(check_prompt(0, Some(0)).warning(), None);
    }
}
