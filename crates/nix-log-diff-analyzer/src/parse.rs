//! Turning the model's raw answer into a verdict.

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The model answered `False`: only environmental noise.
    Inconsequential,
    /// The model answered `True`, followed by an explanation.
    Consequential { explanation: String },
    /// The answer did not start with `True` or `False`.
    Unrecognized,
}

pub fn parse(raw: &str) -> Verdict {
    // Some servers leave the reasoning inline in the content: drop it.
    let answer = raw.split_once("</think>").map_or(raw, |(_, rest)| rest);
    let lines = clean_lines(answer);

    match lines.first() {
        Some(first) if first.starts_with("False") => Verdict::Inconsequential,
        Some(first) if first.starts_with("True") => Verdict::Consequential {
            explanation: lines[1..].join("\n"),
        },
        _ => Verdict::Unrecognized,
    }
}

/// Drop markdown fence lines and blank lines, and normalise whitespace.
fn clean_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.starts_with("```"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn false_is_inconsequential() {
        assert_eq!(parse("False\n"), Verdict::Inconsequential);
    }

    #[test]
    fn true_carries_explanation() {
        assert_eq!(
            parse("True\nA new target appeared.\n\nIt links libfoo.so."),
            Verdict::Consequential {
                explanation: "A new target appeared.\nIt links libfoo.so.".into()
            }
        );
    }

    #[test]
    fn inline_think_block_is_dropped() {
        assert_eq!(
            parse("<think>True? no...</think>\n\nFalse"),
            Verdict::Inconsequential
        );
    }

    #[test]
    fn code_fences_and_indentation_are_stripped() {
        assert_eq!(
            parse("```\n  True\n  Changed   flags\n```"),
            Verdict::Consequential {
                explanation: "Changed flags".into()
            }
        );
    }

    #[test]
    fn garbage_and_empty_are_unrecognized() {
        assert_eq!(parse("Maybe?"), Verdict::Unrecognized);
        assert_eq!(parse(""), Verdict::Unrecognized);
    }
}
