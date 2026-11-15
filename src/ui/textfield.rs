//! Single-line text editing for the address bar, search box and inline rename, drawn into the scene
//! (no native EDIT control, so it renders identically on a remote canvas).

use crate::scene::{Align, Font, Measure, Rect, Scene};
use crate::theme::Theme;

use super::Mods;

pub const VK_BACK: u32 = 0x08;
pub const VK_TAB: u32 = 0x09;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DELETE: u32 = 0x2E;

#[derive(Clone, Default)]
pub struct TextField {
    pub text: String,
    pub caret: usize,
    pub anchor: usize,
    scroll: f32,
}

#[derive(PartialEq, Eq, Debug)]
pub enum FieldAction {
    None,
    Changed,
    Moved,
    Enter,
    Escape,
    Tab,
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(j, _)| j).unwrap_or(0)
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(s.len())
}

fn word_left(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 && s[..j].ends_with(|c: char| !c.is_alphanumeric()) {
        j = prev_boundary(s, j);
    }
    while j > 0 && s[..j].ends_with(|c: char| c.is_alphanumeric()) {
        j = prev_boundary(s, j);
    }
    j
}

fn word_right(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() && s[j..].starts_with(|c: char| c.is_alphanumeric()) {
        j = next_boundary(s, j);
    }
    while j < s.len() && s[j..].starts_with(|c: char| !c.is_alphanumeric()) {
        j = next_boundary(s, j);
    }
    j
}

impl TextField {
    pub fn new(s: &str) -> TextField {
        TextField {
            text: s.to_string(),
            caret: s.len(),
            anchor: s.len(),
            scroll: 0.0,
        }
    }

    pub fn set(&mut self, s: &str) {
        *self = TextField::new(s);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    pub fn select(&mut self, start: usize, end: usize) {
        self.anchor = start.min(self.text.len());
        self.caret = end.min(self.text.len());
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        if a == b {
            return false;
        }
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
        true
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.caret, &s);
        self.caret += s.len();
        self.anchor = self.caret;
    }

    pub fn char(&mut self, c: char) -> FieldAction {
        if c.is_control() {
            return FieldAction::None;
        }
        let mut b = [0u8; 4];
        self.insert(c.encode_utf8(&mut b));
        FieldAction::Changed
    }

    fn move_to(&mut self, i: usize, extend: bool) {
        self.caret = i;
        if !extend {
            self.anchor = i;
        }
    }

    pub fn key(&mut self, vk: u32, m: Mods) -> FieldAction {
        let len = self.text.len();
        match vk {
            VK_RETURN => FieldAction::Enter,
            VK_ESCAPE => FieldAction::Escape,
            VK_TAB => FieldAction::Tab,
            VK_LEFT => {
                let (a, _) = self.selection();
                let i = if m.ctrl {
                    word_left(&self.text, self.caret)
                } else if !m.shift && self.caret != self.anchor {
                    a
                } else {
                    prev_boundary(&self.text, self.caret)
                };
                self.move_to(i, m.shift);
                FieldAction::Moved
            }
            VK_RIGHT => {
                let (_, b) = self.selection();
                let i = if m.ctrl {
                    word_right(&self.text, self.caret)
                } else if !m.shift && self.caret != self.anchor {
                    b
                } else {
                    next_boundary(&self.text, self.caret)
                };
                self.move_to(i, m.shift);
                FieldAction::Moved
            }
            VK_HOME => {
                self.move_to(0, m.shift);
                FieldAction::Moved
            }
            VK_END => {
                self.move_to(len, m.shift);
                FieldAction::Moved
            }
            VK_BACK => {
                if !self.delete_selection() && self.caret > 0 {
                    let i = if m.ctrl {
                        word_left(&self.text, self.caret)
                    } else {
                        prev_boundary(&self.text, self.caret)
                    };
                    self.text.replace_range(i..self.caret, "");
                    self.caret = i;
                    self.anchor = i;
                }
                FieldAction::Changed
            }
            VK_DELETE => {
                if !self.delete_selection() && self.caret < len {
                    let j = if m.ctrl {
                        word_right(&self.text, self.caret)
                    } else {
                        next_boundary(&self.text, self.caret)
                    };
                    self.text.replace_range(self.caret..j, "");
                }
                FieldAction::Changed
            }
            0x41 if m.ctrl => {
                self.select_all();
                FieldAction::Moved
            }
            0x43 if m.ctrl => {
                let (a, b) = self.selection();
                if a != b {
                    crate::platform::set_clipboard_text(&self.text[a..b]);
                }
                FieldAction::None
            }
            0x58 if m.ctrl => {
                let (a, b) = self.selection();
                if a != b {
                    crate::platform::set_clipboard_text(&self.text[a..b]);
                    self.delete_selection();
                    return FieldAction::Changed;
                }
                FieldAction::None
            }
            0x56 if m.ctrl => {
                if let Some(t) = crate::platform::get_clipboard_text() {
                    let line = t.lines().next().unwrap_or("").to_string();
                    self.insert(&line);
                    return FieldAction::Changed;
                }
                FieldAction::None
            }
            _ => FieldAction::None,
        }
    }

    /// Caret index nearest to `x` (DIPs relative to the text's left edge).
    pub fn index_at(&self, x: f32, m: &dyn Measure, font: Font) -> usize {
        let x = x + self.scroll;
        let mut best = (0usize, f32::MAX);
        let mut idx: Vec<usize> = self.text.char_indices().map(|(i, _)| i).collect();
        idx.push(self.text.len());
        for i in idx {
            let w = m.text_width(&self.text[..i], font);
            let d = (w - x).abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    }

    pub fn click(&mut self, x: f32, m: &dyn Measure, font: Font, extend: bool) {
        let i = self.index_at(x, m, font);
        self.move_to(i, extend);
    }

    pub fn drag_to(&mut self, x: f32, m: &dyn Measure, font: Font) {
        self.caret = self.index_at(x, m, font);
    }

    pub fn select_word_at(&mut self, x: f32, m: &dyn Measure, font: Font) {
        let i = self.index_at(x, m, font);
        self.anchor = word_left(&self.text, i.min(self.text.len()));
        let mut j = i;
        while j < self.text.len() && self.text[j..].starts_with(|c: char| c.is_alphanumeric()) {
            j = next_boundary(&self.text, j);
        }
        self.caret = j;
    }

    /// Draws text, selection and caret inside `r` (the text area; no background).
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        s: &mut Scene,
        r: Rect,
        m: &dyn Measure,
        font: Font,
        th: &Theme,
        focused: bool,
        caret_on: bool,
    ) {
        let caret_x = m.text_width(&self.text[..self.caret], font);
        // Keep the caret in view.
        let w = r.w() - 2.0;
        if caret_x - self.scroll > w {
            self.scroll = caret_x - w;
        } else if caret_x < self.scroll {
            self.scroll = caret_x;
        }
        let full = m.text_width(&self.text, font);
        if full - self.scroll < w {
            self.scroll = (full - w).max(0.0);
        }
        let x0 = r.l - self.scroll;
        s.clip(r);
        let (a, b) = self.selection();
        if focused && a != b {
            let xa = x0 + m.text_width(&self.text[..a], font);
            let xb = x0 + m.text_width(&self.text[..b], font);
            s.fill(Rect::new(xa, r.cy() - 9.0, xb, r.cy() + 9.0), th.accent.with_alpha(0.35));
        }
        s.text(
            Rect::new(x0, r.t, x0 + full + 20.0, r.b),
            self.text.clone(),
            font,
            th.text,
            Align::Left,
        );
        if focused && caret_on {
            let x = (x0 + caret_x).round() + 0.5;
            s.line(x, r.cy() - 8.0, x, r.cy() + 8.0, 1.0, th.text);
        }
        s.unclip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut f = TextField::new("hello world");
        let none = Mods::default();
        f.key(VK_HOME, none);
        f.key(VK_RIGHT, Mods { ctrl: true, ..none });
        assert_eq!(f.caret, 6);
        f.key(VK_BACK, none);
        assert_eq!(f.text, "helloworld");
        f.select_all();
        f.char('x');
        assert_eq!(f.text, "x");
        f.char('é');
        f.key(VK_LEFT, none);
        f.key(VK_DELETE, none);
        assert_eq!(f.text, "x");
    }
}
