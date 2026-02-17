use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::{UiIntent, UiModel};

pub fn handle_key(model: &mut UiModel, key: KeyEvent) -> Option<UiIntent> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => return Some(UiIntent::Quit),
        (KeyCode::F(1), _) => return Some(UiIntent::Help),
        (KeyCode::Tab, _) => {
            model.push_log("[ui] tab focus not implemented (single input focus)");
            return None;
        }

        // Push-to-talk
        (KeyCode::Char(' '), KeyModifiers::NONE) => {
            if model.ptt_enabled {
                // treat as momentary press if we can detect press/release; crossterm provides repeats too.
                model.ptt_active = !model.ptt_active;
                return Some(UiIntent::TogglePtt);
            }
            return None;
        }

        // Channel navigation
        (KeyCode::Up, _) => return Some(UiIntent::SelectPrevChannel),
        (KeyCode::Down, _) => return Some(UiIntent::SelectNextChannel),

        // Input editing
        (KeyCode::Enter, _) => {
            let text = model.input.trim().to_string();
            model.input.clear();
            if !text.is_empty() {
                return Some(UiIntent::SendChat { text });
            }
            return None;
        }
        (KeyCode::Backspace, _) => {
            model.input.pop();
            return None;
        }
        (KeyCode::Char(c), KeyModifiers::NONE) => {
            // Simple printable filter
            if !c.is_control() {
                model.input.push(c);
            }
            return None;
        }
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Some(UiIntent::Quit),

        _ => {}
    }

    None
}
