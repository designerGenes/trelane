//! A small, pure, single-line text-input primitive shared by the interactive
//! TUIs (`diagnostic`, `biplane_ui`). It owns a string buffer and a caret
//! position and exposes the handful of edit operations a raw-mode key loop
//! needs. It performs no terminal I/O, so all of its behavior is unit-tested.
//!
//! Callers drive it by translating key events into method calls (insert,
//! backspace, delete, move_left/right, home, end) and read `value()` /
//! `caret()` back for rendering. Committing/cancelling is the caller's concern.

/// A single-line editable text buffer with a caret. Caret is a char index in
/// `0..=chars.len()` (it may sit just past the last char).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    chars: Vec<char>,
    caret: usize,
}

impl TextInput {
    /// New empty input.
    pub fn new() -> Self {
        Self { chars: Vec::new(), caret: 0 }
    }

    /// Pre-populate with existing text; caret starts at the end (natural for
    /// "edit this value" flows).
    pub fn with_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let caret = chars.len();
        Self { chars, caret }
    }

    /// Current string value.
    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    /// Caret position as a char index (0..=len).
    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Insert a character at the caret and advance the caret.
    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.caret, c);
        self.caret += 1;
    }

    /// Insert a whole string at the caret.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }

    /// Delete the char before the caret (Backspace). No-op at the start.
    pub fn backspace(&mut self) {
        if self.caret > 0 {
            self.caret -= 1;
            self.chars.remove(self.caret);
        }
    }

    /// Delete the char at the caret (Delete). No-op at the end.
    pub fn delete(&mut self) {
        if self.caret < self.chars.len() {
            self.chars.remove(self.caret);
        }
    }

    pub fn move_left(&mut self) {
        if self.caret > 0 {
            self.caret -= 1;
        }
    }

    pub fn move_right(&mut self) {
        if self.caret < self.chars.len() {
            self.caret += 1;
        }
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.chars.len();
    }

    /// Clear all text and reset the caret.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.caret = 0;
    }

    /// The value with a visible caret marker inserted (for simple rendering
    /// when a real cursor overlay isn't used). Uses '│' at the caret.
    pub fn render_with_caret(&self) -> String {
        let mut out: String = self.chars[..self.caret].iter().collect();
        out.push('│');
        out.extend(self.chars[self.caret..].iter());
        out
    }
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty_with_caret_zero() {
        let t = TextInput::new();
        assert!(t.is_empty());
        assert_eq!(t.caret(), 0);
        assert_eq!(t.value(), "");
    }

    #[test]
    fn with_text_puts_caret_at_end() {
        let t = TextInput::with_text("hello");
        assert_eq!(t.caret(), 5);
        assert_eq!(t.value(), "hello");
    }

    #[test]
    fn insert_advances_caret() {
        let mut t = TextInput::new();
        t.insert('a');
        t.insert('b');
        assert_eq!(t.value(), "ab");
        assert_eq!(t.caret(), 2);
    }

    #[test]
    fn insert_in_middle() {
        let mut t = TextInput::with_text("ac");
        t.move_left(); // caret between a and c
        t.insert('b');
        assert_eq!(t.value(), "abc");
        assert_eq!(t.caret(), 2);
    }

    #[test]
    fn backspace_removes_before_caret() {
        let mut t = TextInput::with_text("abc");
        t.backspace();
        assert_eq!(t.value(), "ab");
        assert_eq!(t.caret(), 2);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut t = TextInput::with_text("abc");
        t.home();
        t.backspace();
        assert_eq!(t.value(), "abc");
        assert_eq!(t.caret(), 0);
    }

    #[test]
    fn delete_removes_at_caret() {
        let mut t = TextInput::with_text("abc");
        t.home();
        t.delete();
        assert_eq!(t.value(), "bc");
        assert_eq!(t.caret(), 0);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut t = TextInput::with_text("abc");
        t.delete();
        assert_eq!(t.value(), "abc");
    }

    #[test]
    fn caret_movement_is_bounded() {
        let mut t = TextInput::with_text("ab");
        t.move_right(); // already at end
        assert_eq!(t.caret(), 2);
        t.home();
        t.move_left(); // already at start
        assert_eq!(t.caret(), 0);
        t.end();
        assert_eq!(t.caret(), 2);
    }

    #[test]
    fn insert_str_inserts_all() {
        let mut t = TextInput::new();
        t.insert_str("hi there");
        assert_eq!(t.value(), "hi there");
        assert_eq!(t.caret(), 8);
    }

    #[test]
    fn render_with_caret_marks_position() {
        let mut t = TextInput::with_text("abc");
        t.home();
        assert_eq!(t.render_with_caret(), "│abc");
        t.end();
        assert_eq!(t.render_with_caret(), "abc│");
        t.move_left();
        assert_eq!(t.render_with_caret(), "ab│c");
    }

    #[test]
    fn clear_resets() {
        let mut t = TextInput::with_text("abc");
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.caret(), 0);
    }

    #[test]
    fn handles_unicode_by_char_not_byte() {
        let mut t = TextInput::with_text("café");
        assert_eq!(t.len(), 4);
        t.backspace(); // removes 'é' as one char
        assert_eq!(t.value(), "caf");
    }
}
