//! Single-line text input used by forms, prompts and the command line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::Zeroize;

#[derive(Debug, Clone, Default)]
pub struct Input {
    value: String,
    /// Cursor position in characters.
    cursor: usize,
}

impl Drop for Input {
    fn drop(&mut self) {
        // Inputs may hold passwords.
        self.value.zeroize();
    }
}

impl Input {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.value.zeroize();
        self.value = value.into();
        self.cursor = self.value.chars().count();
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.value.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, c: char) {
        let idx = self.byte_index(self.cursor);
        self.value.insert(idx, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.insert(c);
        }
    }

    /// Returns true when the key was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                let idx = self.byte_index(self.cursor);
                self.value.replace_range(..idx, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.value.chars().count(),
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => self.delete_word(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let idx = self.byte_index(self.cursor);
                    self.value.remove(idx);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.value.chars().count() {
                    let idx = self.byte_index(self.cursor);
                    self.value.remove(idx);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            _ => return false,
        }
        true
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let (a, b) = (self.byte_index(start), self.byte_index(self.cursor));
        self.value.replace_range(a..b, "");
        self.cursor = start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn editing() {
        let mut i = Input::new("héllo");
        i.handle_key(key(KeyCode::Left));
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.value(), "hélo");
        i.handle_key(key(KeyCode::Home));
        i.handle_key(key(KeyCode::Char('>')));
        assert_eq!(i.value(), ">hélo");
        i.handle_key(key(KeyCode::End));
        i.insert_str(" world");
        i.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.value(), ">hélo ");
    }
}
