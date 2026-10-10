//! A single-line text editor used by the URL bar, find and form fields.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

pub enum Edit {
    Continue,
    Submit,
    Cancel,
    /// Tab: commit and move on.
    Next,
}

#[derive(Default, Clone)]
pub struct Editor {
    chars: Vec<char>,
    cursor: usize,
    /// Everything is selected (like a browser's address bar on focus): typing replaces it.
    pub selected: bool,
}

impl Editor {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self { cursor: chars.len(), chars, selected: false }
    }

    /// An editor with all its text selected.
    pub fn selected(text: &str) -> Self {
        Self { selected: !text.is_empty(), ..Self::new(text) }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn key(&mut self, key: KeyEvent) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if std::mem::take(&mut self.selected) {
            match key.code {
                KeyCode::Char(_) if !ctrl => self.clear(),
                KeyCode::Backspace | KeyCode::Delete => {
                    self.clear();
                    return Edit::Continue;
                }
                KeyCode::Left | KeyCode::Home => self.cursor = 0,
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => return Edit::Submit,
            KeyCode::Esc => return Edit::Cancel,
            KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return Edit::Cancel,
            KeyCode::Tab => return Edit::Next,
            KeyCode::Left if ctrl => self.cursor = self.word_start(),
            KeyCode::Right if ctrl => self.cursor = self.word_end(),
            KeyCode::Left | KeyCode::Char('b') if ctrl || key.code == KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('f') if ctrl || key.code == KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.chars.len())
            }
            KeyCode::Home | KeyCode::Char('a') if ctrl || key.code == KeyCode::Home => self.cursor = 0,
            KeyCode::End | KeyCode::Char('e') if ctrl || key.code == KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Backspace | KeyCode::Char('h') if ctrl || key.code == KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Delete | KeyCode::Char('d') if ctrl || key.code == KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.chars.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                let start = self.word_start();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            _ => {}
        }
        Edit::Continue
    }

    fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn paste(&mut self, text: &str) {
        if std::mem::take(&mut self.selected) {
            self.clear();
        }
        for c in text.chars().filter(|c| !c.is_control()) {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        let mut i = self.cursor;
        while i < self.chars.len() && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < self.chars.len() && !self.chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// The visible slice for a box `width` columns wide, and the cursor column within it.
    pub fn view(&self, width: usize) -> (String, usize) {
        let w = |c: &char| c.width().unwrap_or(0);
        let before: usize = self.chars[..self.cursor].iter().map(w).sum();
        // Scroll so the cursor stays visible.
        let mut start = 0;
        let mut skipped = 0;
        while before - skipped >= width.max(1) && start < self.cursor {
            skipped += w(&self.chars[start]);
            start += 1;
        }
        let mut out = String::new();
        let mut used = 0;
        for c in &self.chars[start..] {
            if used + w(c) > width {
                break;
            }
            used += w(c);
            out.push(*c);
        }
        (out, before - skipped)
    }
}
