use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Self-implemented single-line input editor (no reedline).
#[derive(Debug, Clone)]
pub struct InputLine {
    pub buffer: String,
    /// Byte offset of cursor within buffer.
    pub cursor: usize,
    pub history: Vec<String>,
    /// None = editing current line; Some(i) = browsing history[i].
    pub history_idx: Option<usize>,
    /// Saved current line when browsing history.
    saved_current: String,
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

impl InputLine {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            saved_current: String::new(),
        }
    }

    /// Returns true if the key was consumed by the editor.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        match key.code {
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.insert_char(c);
                InputAction::Consumed
            }
            KeyCode::Backspace => {
                self.backspace();
                InputAction::Consumed
            }
            KeyCode::Delete => {
                self.delete();
                InputAction::Consumed
            }
            KeyCode::Left => {
                self.move_left();
                InputAction::Consumed
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::Consumed
            }
            KeyCode::Home | KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.cursor = 0;
                InputAction::Consumed
            }
            KeyCode::End | KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                self.cursor = self.buffer.len();
                InputAction::Consumed
            }
            KeyCode::Up => {
                self.history_prev();
                InputAction::Consumed
            }
            KeyCode::Down => {
                self.history_next();
                InputAction::Consumed
            }
            KeyCode::Enter => {
                if !self.buffer.trim().is_empty() {
                    InputAction::Submit
                } else {
                    InputAction::Consumed
                }
            }
            _ => InputAction::NotHandled,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = self.cursor;
        self.buffer.insert(byte_idx, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buffer.len());
        self.buffer.replace_range(self.cursor..next, "");
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.cursor = prev;
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buffer.len());
        self.cursor = next;
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_idx.is_none() {
            self.saved_current = self.buffer.clone();
            self.history_idx = Some(self.history.len() - 1);
        } else if let Some(i) = self.history_idx {
            if i > 0 {
                self.history_idx = Some(i - 1);
            } else {
                return;
            }
        }
        if let Some(i) = self.history_idx {
            self.buffer = self.history[i].clone();
            self.cursor = self.buffer.len();
        }
    }

    pub fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) => {
                if i + 1 >= self.history.len() {
                    self.history_idx = None;
                    self.buffer = std::mem::take(&mut self.saved_current);
                } else {
                    self.history_idx = Some(i + 1);
                    self.buffer = self.history[i + 1].clone();
                }
                self.cursor = self.buffer.len();
            }
        }
    }

    /// Consume the current buffer as submitted text, push to history.
    pub fn take_submitted(&mut self) -> String {
        let text = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_idx = None;
        self.saved_current.clear();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        text
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.saved_current.clear();
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    Consumed,
    Submit,
    NotHandled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn insert_and_backspace_basic() {
        let mut inp = InputLine::new();
        inp.insert_char('h');
        inp.insert_char('i');
        assert_eq!(inp.buffer, "hi");
        assert_eq!(inp.cursor, 2);
        inp.backspace();
        assert_eq!(inp.buffer, "h");
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn cursor_left_right() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 3;
        inp.move_left();
        assert_eq!(inp.cursor, 2);
        inp.move_left();
        inp.move_left();
        assert_eq!(inp.cursor, 0);
        inp.move_left();
        assert_eq!(inp.cursor, 0);
        inp.move_right();
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn insert_in_middle() {
        let mut inp = InputLine::new();
        inp.buffer = "ac".into();
        inp.cursor = 1;
        inp.insert_char('b');
        assert_eq!(inp.buffer, "abc");
        assert_eq!(inp.cursor, 2);
    }

    #[test]
    fn delete_key() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 1;
        inp.delete();
        assert_eq!(inp.buffer, "ac");
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn utf8_boundary_backspace() {
        let mut inp = InputLine::new();
        inp.buffer = "你好".into();
        inp.cursor = "你好".len();
        inp.backspace();
        assert_eq!(inp.buffer, "你");
        assert_eq!(inp.cursor, 3);
    }

    #[test]
    fn history_navigation() {
        let mut inp = InputLine::new();
        inp.history = vec!["first".into(), "second".into()];
        inp.history_prev();
        assert_eq!(inp.buffer, "second");
        inp.history_prev();
        assert_eq!(inp.buffer, "first");
        inp.history_prev();
        assert_eq!(inp.buffer, "first");
        inp.history_next();
        assert_eq!(inp.buffer, "second");
        inp.history_next();
        assert_eq!(inp.history_idx, None);
    }

    #[test]
    fn history_saves_current() {
        let mut inp = InputLine::new();
        inp.history = vec!["old".into()];
        inp.insert_char('n');
        inp.history_prev();
        assert_eq!(inp.buffer, "old");
        inp.history_next();
        assert_eq!(inp.buffer, "n");
    }

    #[test]
    fn take_submitted_pushes_history() {
        let mut inp = InputLine::new();
        inp.buffer = "hello".into();
        let text = inp.take_submitted();
        assert_eq!(text, "hello");
        assert_eq!(inp.history, vec!["hello"]);
        assert!(inp.is_empty());
    }

    #[test]
    fn take_submitted_ignores_empty() {
        let mut inp = InputLine::new();
        inp.buffer = "   ".into();
        let text = inp.take_submitted();
        assert_eq!(text, "   ");
        assert!(
            inp.history.is_empty(),
            "whitespace-only should not be saved"
        );
    }

    #[test]
    fn handle_key_enter_submits() {
        let mut inp = InputLine::new();
        inp.buffer = "hi".into();
        let action = inp.handle_key(key(KeyCode::Enter));
        assert_eq!(action, InputAction::Submit);
    }

    #[test]
    fn handle_key_enter_empty_does_not_submit() {
        let mut inp = InputLine::new();
        let action = inp.handle_key(key(KeyCode::Enter));
        assert_eq!(action, InputAction::Consumed);
    }

    #[test]
    fn handle_key_ctrl_a_home() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 3;
        inp.handle_key(key_ctrl('a'));
        assert_eq!(inp.cursor, 0);
    }

    #[test]
    fn handle_key_ctrl_e_end() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 0;
        inp.handle_key(key_ctrl('e'));
        assert_eq!(inp.cursor, 3);
    }
}
