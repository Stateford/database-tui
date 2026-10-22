//! SQL editor tab: a multi-line editor (with an optional vim layer) and a
//! result grid.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea};

use super::grid::Grid;
use crate::config::KeymapMode;
use crate::keymap::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorFocus {
    Editor,
    Results,
}

pub enum EditorOutcome {
    Handled,
    /// Not an editor key; let the app treat it as an action.
    Action(Action),
    /// Open the vim command line.
    CommandLine,
}

pub struct QueryState {
    pub editor: TextArea<'static>,
    pub vim_mode: VimMode,
    pending: Option<char>,
    pub focus: EditorFocus,
    pub result: Option<Grid>,
    /// Status line under the editor: (text, is_error).
    pub message: Option<(String, bool)>,
    pub running: bool,
    pub seq: u64,
}

impl Default for QueryState {
    fn default() -> Self {
        let mut editor = TextArea::default();
        editor.set_placeholder_text("Write SQL here — F5 or Ctrl-E runs it");
        editor.set_tab_length(2);
        Self {
            editor,
            vim_mode: VimMode::Normal,
            pending: None,
            focus: EditorFocus::Editor,
            result: None,
            message: None,
            running: false,
            seq: 0,
        }
    }
}

impl QueryState {
    pub fn sql(&self) -> String {
        // Run the selection if there is one, otherwise the whole buffer.
        if let Some(((r1, c1), (r2, c2))) = self.editor.selection_range() {
            let lines = self.editor.lines();
            let mut out = String::new();
            for (i, line) in lines.iter().enumerate().take(r2 + 1).skip(r1) {
                let chars: Vec<char> = line.chars().collect();
                let start = if i == r1 { c1 } else { 0 };
                let end = if i == r2 { c2.min(chars.len()) } else { chars.len() };
                out.extend(&chars[start.min(end)..end]);
                if i != r2 {
                    out.push('\n');
                }
            }
            if !out.trim().is_empty() {
                return out;
            }
        }
        self.editor.lines().join("\n")
    }

    pub fn handle_key(&mut self, key: KeyEvent, mode: KeymapMode) -> EditorOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Keys that never reach the text buffer.
        match key.code {
            KeyCode::Tab => return EditorOutcome::Action(Action::FocusNext),
            KeyCode::BackTab => return EditorOutcome::Action(Action::FocusPrev),
            KeyCode::F(1) => return EditorOutcome::Action(Action::Help),
            KeyCode::Char('q') if ctrl => return EditorOutcome::Action(Action::Quit),
            _ => {}
        }
        match mode {
            KeymapMode::Standard => {
                match key.code {
                    KeyCode::Esc => return EditorOutcome::Action(Action::FocusPrev),
                    KeyCode::Char('k') if ctrl => return EditorOutcome::Action(Action::ToggleKeymap),
                    _ => {}
                }
                self.editor.input(key);
                EditorOutcome::Handled
            }
            KeymapMode::Vim => self.vim_key(key),
        }
    }

    fn vim_key(&mut self, key: KeyEvent) -> EditorOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ed = &mut self.editor;
        match self.vim_mode {
            VimMode::Insert => {
                if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
                    self.vim_mode = VimMode::Normal;
                    ed.move_cursor(CursorMove::Back);
                } else {
                    ed.input(key);
                }
                return EditorOutcome::Handled;
            }
            VimMode::Visual => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('v') => {
                        ed.cancel_selection();
                        self.vim_mode = VimMode::Normal;
                    }
                    KeyCode::Char('y') => {
                        ed.copy();
                        self.vim_mode = VimMode::Normal;
                    }
                    KeyCode::Char('d') | KeyCode::Char('x') => {
                        ed.cut();
                        self.vim_mode = VimMode::Normal;
                    }
                    KeyCode::Char('c') => {
                        ed.cut();
                        self.vim_mode = VimMode::Insert;
                    }
                    _ => {
                        if let Some(m) = motion(key) {
                            ed.move_cursor(m);
                        }
                    }
                }
                return EditorOutcome::Handled;
            }
            VimMode::Normal => {}
        }

        if ctrl {
            match key.code {
                KeyCode::Char('r') => {
                    ed.redo();
                }
                KeyCode::Char('d') => (0..10).for_each(|_| ed.move_cursor(CursorMove::Down)),
                KeyCode::Char('u') => (0..10).for_each(|_| ed.move_cursor(CursorMove::Up)),
                KeyCode::Char('h') => return EditorOutcome::Action(Action::FocusSidebar),
                KeyCode::Char('k') => return EditorOutcome::Action(Action::ToggleKeymap),
                _ => {}
            }
            return EditorOutcome::Handled;
        }

        let KeyCode::Char(c) = key.code else {
            if let Some(m) = motion(key) {
                ed.move_cursor(m);
            }
            return EditorOutcome::Handled;
        };

        if let Some(prev) = self.pending.take() {
            match (prev, c) {
                ('g', 'g') => ed.move_cursor(CursorMove::Top),
                ('g', 't') => return EditorOutcome::Action(Action::NextTab),
                ('g', 'T') => return EditorOutcome::Action(Action::PrevTab),
                ('d', 'd') => {
                    select_line(ed);
                    ed.cut();
                }
                ('y', 'y') => {
                    let cursor = ed.cursor();
                    select_line(ed);
                    ed.copy();
                    ed.cancel_selection();
                    ed.move_cursor(CursorMove::Jump(cursor.0 as u16, cursor.1 as u16));
                }
                ('c', 'c') => {
                    ed.move_cursor(CursorMove::Head);
                    ed.delete_line_by_end();
                    self.vim_mode = VimMode::Insert;
                }
                _ => {}
            }
            return EditorOutcome::Handled;
        }

        match c {
            'g' | 'd' | 'y' | 'c' => self.pending = Some(c),
            'i' => self.vim_mode = VimMode::Insert,
            'a' => {
                ed.move_cursor(CursorMove::Forward);
                self.vim_mode = VimMode::Insert;
            }
            'I' => {
                ed.move_cursor(CursorMove::Head);
                self.vim_mode = VimMode::Insert;
            }
            'A' => {
                ed.move_cursor(CursorMove::End);
                self.vim_mode = VimMode::Insert;
            }
            'o' => {
                ed.move_cursor(CursorMove::End);
                ed.insert_newline();
                self.vim_mode = VimMode::Insert;
            }
            'O' => {
                ed.move_cursor(CursorMove::Head);
                ed.insert_newline();
                ed.move_cursor(CursorMove::Up);
                self.vim_mode = VimMode::Insert;
            }
            'v' => {
                ed.start_selection();
                self.vim_mode = VimMode::Visual;
            }
            'V' => {
                ed.move_cursor(CursorMove::Head);
                ed.start_selection();
                ed.move_cursor(CursorMove::End);
                self.vim_mode = VimMode::Visual;
            }
            'x' => {
                ed.delete_next_char();
            }
            'D' => {
                ed.delete_line_by_end();
            }
            'C' => {
                ed.delete_line_by_end();
                self.vim_mode = VimMode::Insert;
            }
            'p' => {
                ed.paste();
            }
            'u' => {
                ed.undo();
            }
            'G' => ed.move_cursor(CursorMove::Bottom),
            ':' => return EditorOutcome::CommandLine,
            '?' => return EditorOutcome::Action(Action::Help),
            _ => {
                if let Some(m) = motion(key) {
                    ed.move_cursor(m);
                }
            }
        }
        EditorOutcome::Handled
    }
}

fn select_line(ed: &mut TextArea<'static>) {
    ed.move_cursor(CursorMove::Head);
    ed.start_selection();
    let before = ed.cursor();
    ed.move_cursor(CursorMove::Down);
    if ed.cursor() == before {
        ed.move_cursor(CursorMove::End);
    }
}

fn motion(key: KeyEvent) -> Option<CursorMove> {
    Some(match key.code {
        KeyCode::Char('h') | KeyCode::Left => CursorMove::Back,
        KeyCode::Char('j') | KeyCode::Down => CursorMove::Down,
        KeyCode::Char('k') | KeyCode::Up => CursorMove::Up,
        KeyCode::Char('l') | KeyCode::Right => CursorMove::Forward,
        KeyCode::Char('w') => CursorMove::WordForward,
        KeyCode::Char('e') => CursorMove::WordEnd,
        KeyCode::Char('b') => CursorMove::WordBack,
        KeyCode::Char('0') | KeyCode::Char('^') | KeyCode::Home => CursorMove::Head,
        KeyCode::Char('$') | KeyCode::End => CursorMove::End,
        KeyCode::Char('}') => CursorMove::ParagraphForward,
        KeyCode::Char('{') => CursorMove::ParagraphBack,
        _ => return None,
    })
}
