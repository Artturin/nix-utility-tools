//! The analysis prompt. The text itself lives in `prompt.txt`; edit it there.

const TEMPLATE: &str = include_str!("prompt.txt");
const DIFF_PLACEHOLDER: &str = "{{DIFF}}";

/// The full prompt with the diff substituted in.
pub fn build(diff: &str) -> String {
    TEMPLATE.replace(DIFF_PLACEHOLDER, diff)
}

/// The prompt with the diff replaced by a short summary (for `--dry-run`).
pub fn preview(diff: &str) -> String {
    let summary = format!(
        "[... diff omitted: {} lines, {} bytes ...]",
        diff.lines().count(),
        diff.len()
    );
    TEMPLATE.replace(DIFF_PLACEHOLDER, &summary)
}
