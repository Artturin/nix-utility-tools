//! Small text helpers for process output.

/// Splits a byte stream into lines as chunks arrive.
#[derive(Default)]
pub struct LineBuffer(Vec<u8>);

impl LineBuffer {
    /// Feed a chunk; `on_line` gets every completed line (without the line
    /// terminator, lossily decoded).
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(String)) {
        for &byte in chunk {
            if byte == b'\n' {
                on_line(self.take());
            } else {
                self.0.push(byte);
            }
        }
    }

    /// The unterminated rest, if any.
    pub fn finish(&mut self, mut on_line: impl FnMut(String)) {
        if !self.0.is_empty() {
            on_line(self.take());
        }
    }

    fn take(&mut self) -> String {
        if self.0.last() == Some(&b'\r') {
            self.0.pop();
        }
        let line = String::from_utf8_lossy(&self.0).into_owned();
        self.0.clear();
        line
    }
}

/// Removes SGR colour sequences (`ESC [ digits;... m`) from a byte stream,
/// also when one is split across chunks.
#[derive(Default)]
pub struct SgrStripper {
    /// A possible sequence seen so far, starting at the ESC.
    pending: Vec<u8>,
}

impl SgrStripper {
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        for &byte in chunk {
            match (self.pending.as_slice(), byte) {
                ([], 0x1b) => self.pending.push(byte),
                ([], _) => out.push(byte),
                ([0x1b], b'[') => self.pending.push(byte),
                ([_, _, ..], b'm') => self.pending.clear(),
                ([_, _, ..], b'0'..=b'9' | b';') => self.pending.push(byte),
                _ => {
                    // Not a colour sequence after all: pass it through.
                    out.append(&mut self.pending);
                    if byte == 0x1b {
                        self.pending.push(byte);
                    } else {
                        out.push(byte);
                    }
                }
            }
        }
    }

    pub fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
    }
}

pub fn strip_sgr(text: &str) -> String {
    let mut stripper = SgrStripper::default();
    let mut out = Vec::new();
    stripper.feed(text.as_bytes(), &mut out);
    stripper.finish(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape for use inside a double-quoted Nix string.
pub fn nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode a file path for use in a URL, leaving `/` alone.
pub fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_across_chunks() {
        let mut buffer = LineBuffer::default();
        let mut lines = Vec::new();
        for chunk in [&b"one\ntw"[..], b"o\r\n\nth", b"ree"] {
            buffer.push(chunk, |l| lines.push(l));
        }
        assert_eq!(lines, ["one", "two", ""]);
        buffer.finish(|l| lines.push(l));
        assert_eq!(lines.last().unwrap(), "three");
    }

    #[test]
    fn sgr_sequences_are_removed_even_when_split() {
        assert_eq!(
            strip_sgr("\x1b[0;36m@@ -1 +1 @@\x1b[0m ok"),
            "@@ -1 +1 @@ ok"
        );

        let mut stripper = SgrStripper::default();
        let mut out = Vec::new();
        for chunk in ["a\x1b", "[1;3", "1mb\x1b[0", "mc"] {
            stripper.feed(chunk.as_bytes(), &mut out);
        }
        stripper.finish(&mut out);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn other_escape_sequences_are_kept() {
        assert_eq!(strip_sgr("\x1b[2Kx\x1b[31"), "\x1b[2Kx\x1b[31");
        assert_eq!(strip_sgr("\x1b\x1b[0mx"), "\x1bx");
    }

    #[test]
    fn nix_strings_and_url_paths_are_escaped() {
        assert_eq!(nix_string(r#"a"b\c${d}$e"#), r#"a\"b\\c\${d}$e"#);
        assert_eq!(url_path("/home/a b/ä"), "/home/a%20b/%C3%A4");
    }
}
