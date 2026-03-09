//! Lightweight inline Markdown parser for "About Me" profile text.
//!
//! Supports: **bold**, *italic*, `code`, [links](url), and :emoji: shortcodes.
//! No headings, images, tables, or HTML — keeps rendering fast and prevents abuse.

use crate::ui::theme;
use eframe::egui;

/// A parsed span of styled text.
enum StyledSpan {
    Normal(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { text: String, url: String },
}

/// Render an "About Me" field with inline markdown into `ui`.
pub fn render_about_me(ui: &mut egui::Ui, text: &str) {
    let spans = parse_inline_markdown(text);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();

    let mut link_urls: Vec<(usize, String)> = Vec::new();

    for span in &spans {
        match span {
            StyledSpan::Normal(t) => {
                job.append(
                    t,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(13.0),
                        color: theme::text_color(),
                        ..Default::default()
                    },
                );
            }
            StyledSpan::Bold(t) => {
                job.append(
                    t,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::new(13.0, egui::FontFamily::Proportional),
                        color: theme::text_color(),
                        ..Default::default()
                    },
                );
            }
            StyledSpan::Italic(t) => {
                job.append(
                    t,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(13.0),
                        color: theme::text_color(),
                        italics: true,
                        ..Default::default()
                    },
                );
            }
            StyledSpan::Code(t) => {
                job.append(
                    t,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::monospace(12.0),
                        color: theme::text_dim(),
                        background: theme::bg_input(),
                        ..Default::default()
                    },
                );
            }
            StyledSpan::Link { text: t, url } => {
                let byte_offset = job.text.len();
                link_urls.push((byte_offset, url.clone()));
                job.append(
                    t,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(13.0),
                        color: theme::COLOR_LINK,
                        underline: egui::Stroke::new(1.0, theme::COLOR_LINK),
                        ..Default::default()
                    },
                );
            }
        }
    }

    let response = ui.label(job);

    // Handle link clicks: if the label was clicked, find which link region was hit.
    if !link_urls.is_empty() {
        if response.interact_pointer_pos().is_some() {
            if response.clicked() {
                // Open the first link for simplicity (profile descriptions are short).
                if let Some((_, url)) = link_urls.first() {
                    let _ = open::that(url);
                }
            }
        }
        // Show pointer cursor on hover.
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
}

/// Parse a limited inline Markdown string into styled spans.
///
/// Recognized syntax (in priority order):
/// - `**bold**`
/// - `*italic*`
/// - `` `code` ``
/// - `[text](url)`
/// - `:emoji_name:` (rendered as-is for now; emoji font handles display)
fn parse_inline_markdown(input: &str) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut buf = String::new();

    while i < len {
        // Inline code: `...`
        if chars[i] == '`' {
            if !buf.is_empty() {
                spans.push(StyledSpan::Normal(std::mem::take(&mut buf)));
            }
            i += 1;
            let mut code = String::new();
            while i < len && chars[i] != '`' {
                code.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing `
            }
            if !code.is_empty() {
                spans.push(StyledSpan::Code(code));
            }
            continue;
        }

        // Bold: **...**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !buf.is_empty() {
                spans.push(StyledSpan::Normal(std::mem::take(&mut buf)));
            }
            i += 2;
            let mut bold = String::new();
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                bold.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip closing **
            }
            if !bold.is_empty() {
                spans.push(StyledSpan::Bold(bold));
            }
            continue;
        }

        // Italic: *...*
        if chars[i] == '*' {
            if !buf.is_empty() {
                spans.push(StyledSpan::Normal(std::mem::take(&mut buf)));
            }
            i += 1;
            let mut italic = String::new();
            while i < len && chars[i] != '*' {
                italic.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing *
            }
            if !italic.is_empty() {
                spans.push(StyledSpan::Italic(italic));
            }
            continue;
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some((text, url, end)) = try_parse_link(&chars, i) {
                if !buf.is_empty() {
                    spans.push(StyledSpan::Normal(std::mem::take(&mut buf)));
                }
                spans.push(StyledSpan::Link { text, url });
                i = end;
                continue;
            }
        }

        // Emoji shortcode: :name: — pass through as-is (rendered by system font).
        if chars[i] == ':' {
            if let Some((emoji_text, end)) = try_parse_emoji(&chars, i) {
                if !buf.is_empty() {
                    spans.push(StyledSpan::Normal(std::mem::take(&mut buf)));
                }
                spans.push(StyledSpan::Normal(emoji_text));
                i = end;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }

    if !buf.is_empty() {
        spans.push(StyledSpan::Normal(buf));
    }

    spans
}

/// Try to parse `[text](url)` starting at position `start`.
/// Returns (text, url, end_index) or None.
fn try_parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let len = chars.len();
    if start >= len || chars[start] != '[' {
        return None;
    }

    let mut i = start + 1;
    let mut text = String::new();
    while i < len && chars[i] != ']' {
        if chars[i] == '\n' {
            return None;
        }
        text.push(chars[i]);
        i += 1;
    }
    if i >= len {
        return None;
    }
    i += 1; // skip ]

    if i >= len || chars[i] != '(' {
        return None;
    }
    i += 1; // skip (

    let mut url = String::new();
    while i < len && chars[i] != ')' {
        if chars[i] == '\n' {
            return None;
        }
        url.push(chars[i]);
        i += 1;
    }
    if i >= len {
        return None;
    }
    i += 1; // skip )

    // Only allow https:// URLs for safety.
    if !url.starts_with("https://") {
        return None;
    }

    if text.is_empty() || url.is_empty() {
        return None;
    }

    Some((text, url, i))
}

/// Try to parse `:emoji_name:` starting at position `start`.
/// Returns the shortcode text (including colons) and end index, or None.
fn try_parse_emoji(chars: &[char], start: usize) -> Option<(String, usize)> {
    let len = chars.len();
    if start >= len || chars[start] != ':' {
        return None;
    }

    let mut i = start + 1;
    let mut name = String::new();
    // Emoji shortcodes: alphanumeric + underscore, max 32 chars.
    while i < len && name.len() < 32 {
        let c = chars[i];
        if c == ':' {
            if name.len() >= 2 {
                let result = format!(":{name}:");
                return Some((result, i + 1));
            }
            return None;
        }
        if c.is_alphanumeric() || c == '_' || c == '+' || c == '-' {
            name.push(c);
            i += 1;
        } else {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_only() {
        let spans = parse_inline_markdown("Hello world");
        assert_eq!(spans.len(), 1);
        assert!(matches!(&spans[0], StyledSpan::Normal(t) if t == "Hello world"));
    }

    #[test]
    fn bold_and_italic() {
        let spans = parse_inline_markdown("a **bold** b *italic* c");
        assert_eq!(spans.len(), 5);
        assert!(matches!(&spans[0], StyledSpan::Normal(t) if t == "a "));
        assert!(matches!(&spans[1], StyledSpan::Bold(t) if t == "bold"));
        assert!(matches!(&spans[2], StyledSpan::Normal(t) if t == " b "));
        assert!(matches!(&spans[3], StyledSpan::Italic(t) if t == "italic"));
        assert!(matches!(&spans[4], StyledSpan::Normal(t) if t == " c"));
    }

    #[test]
    fn inline_code() {
        let spans = parse_inline_markdown("use `code` here");
        assert_eq!(spans.len(), 3);
        assert!(matches!(&spans[1], StyledSpan::Code(t) if t == "code"));
    }

    #[test]
    fn link_parsing() {
        let spans = parse_inline_markdown("see [my site](https://example.com) ok");
        assert_eq!(spans.len(), 3);
        assert!(
            matches!(&spans[1], StyledSpan::Link { text, url } if text == "my site" && url == "https://example.com")
        );
    }

    #[test]
    fn rejects_http_links() {
        let spans = parse_inline_markdown("[bad](http://evil.com)");
        // Should not parse as a link — falls through as normal text.
        assert_eq!(spans.len(), 1);
        assert!(matches!(&spans[0], StyledSpan::Normal(_)));
    }

    #[test]
    fn emoji_shortcode() {
        let spans = parse_inline_markdown("hello :smile: world");
        assert_eq!(spans.len(), 3);
        assert!(matches!(&spans[1], StyledSpan::Normal(t) if t == ":smile:"));
    }
}
