//! ANSI SGR escape sequences to styled spans, for showing diffoscope's
//! coloured output in a text buffer. Every other escape sequence is dropped.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// 0-255 of the xterm palette (0-15 are the standard and bright colours).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn rgb(self) -> (u8, u8, u8) {
        const BASE: [(u8, u8, u8); 16] = [
            (0x2e, 0x34, 0x36),
            (0xcc, 0x00, 0x00),
            (0x4e, 0x9a, 0x06),
            (0xc4, 0xa0, 0x00),
            (0x34, 0x65, 0xa4),
            (0x75, 0x50, 0x7b),
            (0x06, 0x98, 0x9a),
            (0xd3, 0xd7, 0xcf),
            (0x55, 0x57, 0x53),
            (0xef, 0x29, 0x29),
            (0x8a, 0xe2, 0x34),
            (0xfc, 0xe9, 0x4f),
            (0x72, 0x9f, 0xcf),
            (0xad, 0x7f, 0xa8),
            (0x34, 0xe2, 0xe2),
            (0xee, 0xee, 0xec),
        ];
        match self {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Indexed(i @ 0..=15) => BASE[usize::from(i)],
            Color::Indexed(i @ 16..=231) => {
                let i = i - 16;
                let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                (level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            Color::Indexed(i) => {
                let gray = 8 + 10 * (i - 232);
                (gray, gray, gray)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

pub fn parse(input: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = input.chars().peekable();

    let flush = |text: &mut String, style: Style, spans: &mut Vec<Span>| {
        if text.is_empty() {
            return;
        }
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => spans.push(Span {
                text: std::mem::take(text),
                style,
            }),
        }
        text.clear();
    };

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if final_byte == Some('m') {
                    flush(&mut text, style, &mut spans);
                    apply_sgr(&mut style, &params);
                }
            }
            // Operating system command: dropped up to BEL or ST (ESC \).
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            // Some other escape: drop the introducer and what it names.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    flush(&mut text, style, &mut spans);
    spans
}

fn apply_sgr(style: &mut Style, params: &str) {
    let mut codes = params.split([';', ':']).map(|p| {
        if p.is_empty() {
            Some(0)
        } else {
            p.parse::<u16>().ok()
        }
    });
    while let Some(code) = codes.next() {
        let Some(code) = code else { continue };
        match code {
            0 => *style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = Some(Color::Indexed((code - 30) as u8)),
            38 | 48 => {
                let color = match codes.next().flatten() {
                    Some(5) => codes
                        .next()
                        .flatten()
                        .map(|n| Color::Indexed(n.min(255) as u8)),
                    Some(2) => {
                        let mut channel = || codes.next().flatten().map(|n| n.min(255) as u8);
                        match (channel(), channel(), channel()) {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r, g, b)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(color) = color {
                    if code == 38 {
                        style.fg = Some(color);
                    } else {
                        style.bg = Some(color);
                    }
                }
            }
            39 => style.fg = None,
            40..=47 => style.bg = Some(Color::Indexed((code - 40) as u8)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(Color::Indexed((code - 90 + 8) as u8)),
            100..=107 => style.bg = Some(Color::Indexed((code - 100 + 8) as u8)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn plain_text_is_one_span() {
        let spans = parse("│ hello\n├── world\n");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(plain(&spans), "│ hello\n├── world\n");
    }

    #[test]
    fn colours_and_reset() {
        let spans = parse("a\x1b[0;36m@@ -1 +1 @@\x1b[0m b");
        assert_eq!(plain(&spans), "a@@ -1 +1 @@ b");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].style.fg, Some(Color::Indexed(6)));
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn attributes_accumulate_and_clear() {
        let spans = parse("\x1b[1;31mx\x1b[22my\x1b[mz");
        assert!(spans[0].style.bold && spans[0].style.fg.is_some());
        assert!(!spans[1].style.bold && spans[1].style.fg.is_some());
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn extended_colours() {
        let spans = parse("\x1b[38;5;196mr\x1b[48;2;1;2;3mg\x1b[39;49mn");
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(196)));
        assert_eq!(spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(spans[2].style, Style::default());
        assert_eq!(Color::Indexed(196).rgb(), (255, 0, 0));
        assert_eq!(Color::Indexed(232).rgb(), (8, 8, 8));
    }

    #[test]
    fn other_sequences_and_truncation_are_dropped() {
        assert_eq!(plain(&parse("a\x1b[2Kb\x1b]0;title\x07c")), "abc");
        assert_eq!(
            plain(&parse("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\")),
            "alink"
        );
        assert_eq!(plain(&parse("end\x1b[3")), "end");
        assert_eq!(plain(&parse("end\x1b")), "end");
    }

    #[test]
    fn bright_colours() {
        let spans = parse("\x1b[91mx");
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(9)));
    }
}
