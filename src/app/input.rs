//! Single-line text input used by forms, prompts and the command line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::Zeroize;

/// Inputs may hold passwords, so they are wiped when dropped or replaced,
/// leave no copies behind when they grow, and are not printed by `Debug`.
#[derive(Default)]
pub struct Input {
    value: String,
    /// Cursor position in characters.
    cursor: usize,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input").field("cursor", &self.cursor).finish_non_exhaustive()
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

    /// Makes room for `additional` bytes. A `String` that grows frees its
    /// old buffer as it is, so the input moves to a bigger one itself and
    /// wipes the old one.
    fn reserve(&mut self, additional: usize) {
        if self.value.capacity() - self.value.len() >= additional {
            return;
        }
        let capacity = (self.value.len() + additional).max(self.value.capacity() * 2).max(32);
        let mut grown = String::with_capacity(capacity);
        grown.push_str(&self.value);
        self.value.zeroize();
        self.value = grown;
    }

    pub fn insert(&mut self, c: char) {
        self.reserve(c.len_utf8());
        let idx = self.byte_index(self.cursor);
        self.value.insert(idx, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.reserve(s.len());
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

    /// Inputs hold passwords: they are never printed, and growing one does
    /// not leave its earlier content behind in a freed buffer.
    #[test]
    fn secrets_stay_inside() {
        let mut i = Input::default();
        let mut buffer = (std::ptr::null(), 0);
        let mut moves = 0;
        for c in "correct horse battery staple, and then some more to grow".chars() {
            i.insert(c);
            if i.value.as_ptr() != buffer.0 {
                buffer = (i.value.as_ptr(), i.value.capacity());
                moves += 1;
            }
            assert!(i.value.capacity() >= i.value.len());
        }
        assert_eq!(i.value(), "correct horse battery staple, and then some more to grow");
        // Grown by the input itself (32, then 64 bytes), which wipes the old
        // buffer, rather than by `String` (8, 16, 32, 64), which does not.
        assert_eq!((moves, buffer.1), (2, 64));
        let shown = format!("{i:?}");
        assert!(!shown.contains("horse"), "{shown}");
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
