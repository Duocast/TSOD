#[derive(Debug, Clone, Copy)]
pub enum ComposerFormatAction {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    OrderedList,
    UnorderedList,
    Quote,
    CodeBlock,
}

#[derive(Default)]
pub struct ChatComposer {
    text: String,
}

impl ChatComposer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }

    pub fn text(&self) -> String {
        self.text.clone()
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }

    pub fn apply_format_action(&mut self, action: ComposerFormatAction) {
        match action {
            ComposerFormatAction::Bold => self.text.push_str("****"),
            ComposerFormatAction::Italic => self.text.push_str("**"),
            ComposerFormatAction::Underline => self.text.push_str("<u></u>"),
            ComposerFormatAction::Strikethrough => self.text.push_str("~~~~"),
            ComposerFormatAction::OrderedList => self.text.push_str("1. "),
            ComposerFormatAction::UnorderedList => self.text.push_str("- "),
            ComposerFormatAction::Quote => self.text.push_str("> "),
            ComposerFormatAction::CodeBlock => self.text.push_str("```\n\n```"),
        }
    }
}
