//! Discord-style Markdown for messages: **bold**, *italic*, __underline__, ~~strike~~,
//! `code`, ```code blocks```, > quotes, # headings, - lists and -# small text.
//! A small hand-written parser keeps the app light; messages with no Markdown
//! characters skip it entirely. Links, @mentions and emoji inside styled text still work.

use iced::widget::text::Span;
use iced::widget::{column, container, rich_text, row, span, text};
use iced::{Border, Color, Element, Font, Length, Padding, font};

use crate::app::{App, Message, Session};
use crate::view_messages::{Piece, link_end, pieces};

/// Styles that apply to a run of inline text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Seg>),
    /// `#`, `##` or `###`.
    Heading(u8, Vec<Seg>),
    /// `-# small grey text`.
    Subtext(Vec<Seg>),
    /// `- item` or `* item`, with its nesting depth.
    ListItem(u8, Vec<Seg>),
    /// `> quoted` lines (consecutive lines are one quote).
    Quote(Vec<Block>),
    Code(String),
}

/// True if the text might contain Markdown, so plain messages skip the parser.
pub fn might_have_markdown(s: &str) -> bool {
    s.contains(['*', '_', '~', '`', '>', '#', '-'])
}

const MARKERS: [&str; 6] = ["**", "__", "~~", "*", "_", "`"];

/// Parse inline styles.
pub fn inline(s: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    parse_inline(s, Style::default(), &mut out);
    // Merge neighbours with the same style.
    let mut merged: Vec<Seg> = Vec::new();
    for seg in out {
        match merged.last_mut() {
            Some(last) if last.style == seg.style => last.text.push_str(&seg.text),
            _ if seg.text.is_empty() => {}
            _ => merged.push(seg),
        }
    }
    merged
}

fn push(out: &mut Vec<Seg>, text: &str, style: Style) {
    if !text.is_empty() {
        out.push(Seg { text: text.to_string(), style });
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

fn parse_inline(s: &str, style: Style, out: &mut Vec<Seg>) {
    let mut plain_start = 0;
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        // Links are left whole (underscores in URLs aren't italics).
        if rest.starts_with("https://") || rest.starts_with("http://") {
            i += link_end(rest).max(1);
            continue;
        }
        // Backslash escapes a marker character.
        if let Some(after) = rest.strip_prefix('\\')
            && let Some(c) = after.chars().next()
            && "*_~`>#-\\|".contains(c)
        {
            push(out, &s[plain_start..i], style);
            push(out, &c.to_string(), style);
            i += 1 + c.len_utf8();
            plain_start = i;
            continue;
        }
        let Some(marker) = MARKERS.iter().find(|m| rest.starts_with(**m)) else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let before = s[..i].chars().next_back();
        let open_end = i + marker.len();
        // `_italic_` only at word edges, so snake_case stays as it is.
        if *marker == "_" && is_word(before) {
            i = open_end;
            continue;
        }
        if let Some(close) = find_close(s, open_end, marker) {
            let inner = &s[open_end..close];
            let after = s[close + marker.len()..].chars().next();
            let ok = !inner.is_empty()
                && !inner.starts_with(' ')
                && !(*marker == "_" && is_word(after))
                && (marker.len() == 2 || *marker == "`" || !inner.ends_with(' '));
            if ok {
                push(out, &s[plain_start..i], style);
                let mut inner_style = style;
                match *marker {
                    "**" => inner_style.bold = true,
                    "__" => inner_style.underline = true,
                    "~~" => inner_style.strike = true,
                    "*" | "_" => inner_style.italic = true,
                    _ => inner_style.code = true,
                }
                if *marker == "`" {
                    push(out, inner, inner_style);
                } else {
                    parse_inline(inner, inner_style, out);
                }
                i = close + marker.len();
                plain_start = i;
                continue;
            }
        }
        i = open_end;
    }
    push(out, &s[plain_start..], style);
}

/// Where the closing `marker` starts, searching from `from`.
fn find_close(s: &str, from: usize, marker: &str) -> Option<usize> {
    let mut j = from;
    while j < s.len() {
        let rest = &s[j..];
        if marker != "`" && rest.starts_with('\\') {
            j += 1 + rest[1..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if marker == "`" {
            return rest.find('`').map(|k| j + k);
        }
        if rest.starts_with(marker) {
            // A single `*` must not be half of a `**`.
            let single = marker.len() == 1;
            if single && rest[1..].starts_with(marker) {
                j += 2;
                continue;
            }
            // In `***`, a double marker closes on the last two characters.
            if !single && rest[2..].starts_with(&marker[..1]) {
                j += 1;
                continue;
            }
            return Some(j);
        }
        // Skip over code spans: markers inside them don't count.
        if rest.starts_with('`')
            && let Some(k) = rest[1..].find('`')
        {
            j += k + 2;
            continue;
        }
        j += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Split a message into blocks.
pub fn blocks(s: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut lines = s.lines().peekable();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Block>| {
        if !para.is_empty() {
            out.push(Block::Paragraph(inline(&para.join("\n"))));
            para.clear();
        }
    };
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(after) = trimmed.strip_prefix("```") {
            // A code block runs to the closing ``` (or the end of the message).
            flush(&mut para, &mut out);
            let mut code = Vec::new();
            if let Some(end) = after.find("```") {
                out.push(Block::Code(after[..end].to_string()));
                continue;
            }
            // Text after the opening fence is a language name if it's one word.
            if !after.trim().is_empty() && after.trim().contains(' ') {
                code.push(after);
            }
            for l in lines.by_ref() {
                if let Some(end) = l.find("```") {
                    if !l[..end].is_empty() {
                        code.push(&l[..end]);
                    }
                    break;
                }
                code.push(l);
            }
            out.push(Block::Code(code.join("\n")));
        } else if let Some(q) = trimmed.strip_prefix("> ").or_else(|| (trimmed == ">").then_some("")) {
            flush(&mut para, &mut out);
            let mut quoted = vec![q];
            while let Some(next) = lines.peek() {
                let nt = next.trim_start();
                match nt.strip_prefix("> ").or_else(|| (nt == ">").then_some("")) {
                    Some(q) => {
                        quoted.push(q);
                        lines.next();
                    }
                    None => break,
                }
            }
            out.push(Block::Quote(blocks(&quoted.join("\n"))));
        } else if let Some((level, rest)) = heading(trimmed) {
            flush(&mut para, &mut out);
            out.push(Block::Heading(level, inline(rest)));
        } else if let Some(rest) = trimmed.strip_prefix("-# ") {
            flush(&mut para, &mut out);
            out.push(Block::Subtext(inline(rest)));
        } else if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            flush(&mut para, &mut out);
            let depth = ((line.len() - trimmed.len()) / 2).min(3) as u8;
            out.push(Block::ListItem(depth, inline(rest)));
        } else {
            para.push(line);
        }
    }
    flush(&mut para, &mut out);
    out
}

fn heading(line: &str) -> Option<(u8, &str)> {
    for (prefix, level) in [("### ", 3), ("## ", 2), ("# ", 1)] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((level, rest));
        }
    }
    None
}

/// Colours used to draw Markdown.
#[derive(Clone, Copy)]
struct Palette {
    text: Color,
    muted: Color,
    code_bg: Color,
    quote_bar: Color,
}

impl App {
    /// Message text with Markdown, links, @mentions and emoji.
    pub(crate) fn markdown_text<'a>(&'a self, s: &'a Session, content: &str, size: f32) -> Element<'a, Message> {
        let t = &self.theme;
        let p = Palette {
            text: t.text.0,
            muted: t.muted.0,
            code_bg: crate::style::darken(t.chat.0, 0.25),
            quote_bar: crate::theme::with_alpha(t.muted.0, 0.55),
        };
        self.render_blocks(s, &blocks(content), size, crate::fonts::ui(), p)
    }

    fn md_spans<'a>(&'a self, s: &'a Session, segs: &[Seg], base: Font, p: Palette) -> Vec<Span<'a, Message, Font>> {
        let mut out = Vec::new();
        for seg in segs {
            let st = seg.style;
            if st.code {
                out.push(
                    span(seg.text.clone())
                        .font(Font::MONOSPACE)
                        .color(p.text)
                        .background(p.code_bg)
                        .border(Border { radius: 3.0.into(), ..Default::default() })
                        .padding(Padding { left: 2.0, right: 2.0, ..Padding::ZERO }),
                );
                continue;
            }
            let font = Font {
                weight: if st.bold { font::Weight::Bold } else { base.weight },
                style: if st.italic { font::Style::Italic } else { font::Style::Normal },
                ..base
            };
            for piece in pieces(&seg.text) {
                // Plain text and links take the style; emoji and mention pills keep their own look.
                let styled = matches!(piece, Piece::Text(_) | Piece::Link(_));
                let mut sp = self.piece_span(s, piece);
                if styled {
                    sp = sp.font(font);
                }
                out.push(sp.underline(st.underline).strikethrough(st.strike));
            }
        }
        out
    }

    fn md_paragraph<'a>(
        &'a self,
        s: &'a Session,
        segs: &[Seg],
        size: f32,
        base: Font,
        color: Color,
        p: Palette,
    ) -> Element<'a, Message> {
        rich_text(self.md_spans(s, segs, base, p)).size(size).color(color).on_link_click(|m| m).into()
    }

    fn render_blocks<'a>(
        &'a self,
        s: &'a Session,
        blocks: &[Block],
        size: f32,
        base: Font,
        p: Palette,
    ) -> Element<'a, Message> {
        let mut col = column![].spacing(4);
        for b in blocks {
            let el: Element<'a, Message> = match b {
                Block::Paragraph(segs) => self.md_paragraph(s, segs, size, base, p.text, p),
                Block::Heading(level, segs) => {
                    let scale = match level {
                        1 => 1.5,
                        2 => 1.3,
                        _ => 1.12,
                    };
                    let bold = Font { weight: font::Weight::Bold, ..base };
                    self.md_paragraph(s, segs, size * scale, bold, p.text, p)
                }
                Block::Subtext(segs) => self.md_paragraph(s, segs, size * 0.8, base, p.muted, p),
                Block::ListItem(depth, segs) => row![
                    text(if *depth == 0 { "•" } else { "◦" }).size(size).color(p.muted),
                    self.md_paragraph(s, segs, size, base, p.text, p)
                ]
                .spacing(8)
                .padding(Padding { left: 6.0 + *depth as f32 * 18.0, ..Padding::ZERO })
                .into(),
                Block::Quote(inner) => row![
                    container(text("")).width(4).height(Length::Fill).style(move |_| container::Style {
                        background: Some(p.quote_bar.into()),
                        border: Border { radius: 2.0.into(), ..Default::default() },
                        ..Default::default()
                    }),
                    self.render_blocks(s, inner, size, base, p),
                ]
                .spacing(10)
                .height(Length::Shrink)
                .into(),
                Block::Code(code) => container(text(code.clone()).font(Font::MONOSPACE).size(size * 0.9).color(p.text))
                    .padding([8, 10])
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(p.code_bg.into()),
                        border: Border { radius: 4.0.into(), width: 1.0, color: p.quote_bar },
                        ..Default::default()
                    })
                    .into(),
            };
            col = col.push(el);
        }
        col.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(s: &str) -> Vec<(String, Style)> {
        inline(s).into_iter().map(|seg| (seg.text, seg.style)).collect()
    }
    const B: Style = Style { bold: true, italic: false, underline: false, strike: false, code: false };
    const I: Style = Style { bold: false, italic: true, underline: false, strike: false, code: false };
    const N: Style = Style { bold: false, italic: false, underline: false, strike: false, code: false };

    #[test]
    fn inline_styles() {
        assert_eq!(styled("a **b** c"), vec![("a ".into(), N), ("b".into(), B), (" c".into(), N)]);
        assert_eq!(styled("*it*"), vec![("it".into(), I)]);
        assert_eq!(styled("_it_"), vec![("it".into(), I)]);
        let bi = styled("***both***");
        assert!(bi[0].1.bold && bi[0].1.italic && bi[0].0 == "both");
        assert!(styled("__u__")[0].1.underline);
        assert!(styled("~~s~~")[0].1.strike);
        let c = styled("`**not bold**`");
        assert!(c[0].1.code && !c[0].1.bold && c[0].0 == "**not bold**");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        assert_eq!(styled("snake_case_name"), vec![("snake_case_name".into(), N)]);
        assert_eq!(styled("2 * 3 * 4"), vec![("2 * 3 * 4".into(), N)]);
        assert_eq!(styled("unclosed **bold"), vec![("unclosed **bold".into(), N)]);
        assert_eq!(styled(r"\*literal\*"), vec![("*literal*".into(), N)]);
        assert_eq!(styled("a-b - c # d > e"), vec![("a-b - c # d > e".into(), N)]);
    }

    #[test]
    fn block_structure() {
        let b = blocks("# Title\nhello\n> quoted\n> more\n- one\n  - two\n-# fine print\n```\nlet x = 1;\n```");
        assert!(matches!(&b[0], Block::Heading(1, _)));
        assert!(matches!(&b[1], Block::Paragraph(_)));
        let Block::Quote(q) = &b[2] else { panic!("{b:?}") };
        assert_eq!(q.len(), 1);
        assert!(matches!(&b[3], Block::ListItem(0, _)));
        assert!(matches!(&b[4], Block::ListItem(1, _)));
        assert!(matches!(&b[5], Block::Subtext(_)));
        assert_eq!(b[6], Block::Code("let x = 1;".into()));
        assert_eq!(blocks("```rust\nfn main() {}\n```"), vec![Block::Code("fn main() {}".into())]);
        assert_eq!(blocks("```inline```"), vec![Block::Code("inline".into())]);
        assert!(!might_have_markdown("hello there!"));
    }

    #[test]
    fn links_keep_their_underscores() {
        let segs = inline("see https://example.com/_a_b_ and _this_");
        assert_eq!(segs[0].text, "see https://example.com/_a_b_ and ");
        assert!(segs[1].style.italic && segs[1].text == "this");
    }
}
