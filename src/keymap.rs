//! Translates key presses into [`Action`]s for the two key schemes:
//! a "standard" scheme (arrows, function keys, Ctrl shortcuts) and a
//! vim-like scheme (hjkl, counts-free motions, `dd`, `yy`, `:` commands).
//! Arrow keys, Enter, Esc and Tab work in both.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeymapMode;
use crate::db::Privileges;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Connections,
    Sidebar,
    Grid,
    Structure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Top,
    Bottom,
    Home,
    End,
    Select,
    Back,
    Quit,
    Help,
    ToggleKeymap,
    CommandLine,
    FocusNext,
    FocusPrev,
    FocusSidebar,
    FocusMain,
    TabData,
    TabStructure,
    TabQuery,
    NextTab,
    PrevTab,
    // Grid
    Edit,
    View,
    SetNull,
    DeleteRow,
    InsertRow,
    Undo,
    Commit,
    Refresh,
    Copy,
    Paste,
    NextPage,
    PrevPage,
    Filter,
    Sort,
    // Connection list
    New,
    EditItem,
    DeleteItem,
}

/// Stateful resolver: vim bindings such as `gg` or `dd` span two keys.
#[derive(Debug, Default)]
pub struct Keymap {
    pending: Option<char>,
}

impl Keymap {
    pub fn pending(&self) -> Option<char> {
        self.pending
    }

    pub fn clear(&mut self) {
        self.pending = None;
    }

    pub fn resolve(&mut self, mode: KeymapMode, ctx: Context, key: KeyEvent) -> Option<Action> {
        if let Some(action) = common(ctx, key) {
            self.pending = None;
            return Some(action);
        }
        match mode {
            KeymapMode::Standard => standard(ctx, key),
            KeymapMode::Vim => self.vim(ctx, key),
        }
    }

    fn vim(&mut self, ctx: Context, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let KeyCode::Char(c) = key.code else {
            self.pending = None;
            return None;
        };
        if ctrl {
            self.pending = None;
            return match c {
                'd' => Some(Action::HalfPageDown),
                'u' => Some(Action::HalfPageUp),
                'f' => Some(Action::PageDown),
                'b' => Some(Action::PageUp),
                'h' => Some(Action::FocusSidebar),
                'l' => Some(Action::FocusMain),
                'r' => Some(Action::Refresh),
                's' => Some(Action::Commit),
                'k' => Some(Action::ToggleKeymap),
                'q' => Some(Action::Quit),
                _ => None,
            };
        }
        if let Some(prev) = self.pending.take() {
            return match (prev, c) {
                ('g', 'g') => Some(Action::Top),
                ('g', 't') => Some(Action::NextTab),
                ('g', 'T') => Some(Action::PrevTab),
                ('d', 'd') if ctx == Context::Grid => Some(Action::DeleteRow),
                ('d', 'd') if ctx == Context::Connections => Some(Action::DeleteItem),
                ('y', 'y') if ctx == Context::Grid => Some(Action::Copy),
                _ => None,
            };
        }
        let action = match c {
            'g' => {
                self.pending = Some(c);
                return None;
            }
            'd' if matches!(ctx, Context::Grid | Context::Connections) => {
                self.pending = Some(c);
                return None;
            }
            'y' if ctx == Context::Grid => {
                self.pending = Some(c);
                return None;
            }
            'j' => Action::Down,
            'k' => Action::Up,
            'h' => Action::Left,
            'l' => Action::Right,
            'G' => Action::Bottom,
            '0' | '^' => Action::Home,
            '$' => Action::End,
            'w' | 'e' if ctx == Context::Grid => Action::Right,
            'b' if ctx == Context::Grid => Action::Left,
            ':' => Action::CommandLine,
            '?' => Action::Help,
            '/' => Action::Filter,
            'q' if ctx == Context::Connections => Action::Quit,
            'q' => Action::Back,
            'n' if ctx == Context::Connections => Action::New,
            'e' if ctx == Context::Connections => Action::EditItem,
            'i' | 'a' | 'c' if ctx == Context::Grid => Action::Edit,
            'v' if ctx == Context::Grid => Action::View,
            'x' if ctx == Context::Grid => Action::SetNull,
            'o' if ctx == Context::Grid => Action::InsertRow,
            'u' if ctx == Context::Grid => Action::Undo,
            'p' if ctx == Context::Grid => Action::Paste,
            's' if ctx == Context::Grid => Action::Sort,
            'R' => Action::Refresh,
            ']' => Action::NextPage,
            '[' => Action::PrevPage,
            _ => return None,
        };
        Some(action)
    }
}

/// Bindings shared by both schemes.
fn common(ctx: Context, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    Some(match key.code {
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Left if alt => Action::PrevPage,
        KeyCode::Right if alt => Action::NextPage,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home if ctrl => Action::Top,
        KeyCode::End if ctrl => Action::Bottom,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Enter => Action::Select,
        KeyCode::Esc => Action::Back,
        KeyCode::Tab => Action::FocusNext,
        KeyCode::BackTab => Action::FocusPrev,
        KeyCode::F(1) => Action::Help,
        KeyCode::F(5) => Action::Refresh,
        KeyCode::Char('1') if ctx != Context::Connections => Action::TabData,
        KeyCode::Char('2') if ctx != Context::Connections => Action::TabStructure,
        KeyCode::Char('3') if ctx != Context::Connections => Action::TabQuery,
        _ => return None,
    })
}

fn standard(ctx: Context, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let grid = ctx == Context::Grid;
    let conns = ctx == Context::Connections;
    Some(match key.code {
        KeyCode::Char('q') if ctrl => Action::Quit,
        KeyCode::Char('k') if ctrl => Action::ToggleKeymap,
        KeyCode::Char('s') if ctrl => Action::Commit,
        KeyCode::Char('r') if ctrl => Action::Refresh,
        KeyCode::Char('f') if ctrl => Action::Filter,
        KeyCode::Char('/') => Action::Filter,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('z') if ctrl && grid => Action::Undo,
        KeyCode::Char('c') if ctrl && grid => Action::Copy,
        KeyCode::Char('v') if ctrl && grid => Action::Paste,
        KeyCode::Char('n') if ctrl && grid => Action::InsertRow,
        KeyCode::Char('o') if ctrl && grid => Action::Sort,
        KeyCode::F(2) if grid => Action::Edit,
        KeyCode::F(3) if grid => Action::View,
        KeyCode::F(6) if grid => Action::Sort,
        KeyCode::Char(' ') if grid => Action::View,
        KeyCode::Delete if grid => Action::DeleteRow,
        KeyCode::Insert if grid => Action::InsertRow,
        KeyCode::Backspace if grid => Action::SetNull,
        KeyCode::Char(']') => Action::NextPage,
        KeyCode::Char('[') => Action::PrevPage,
        KeyCode::Char('n') if ctrl && conns => Action::New,
        KeyCode::Insert if conns => Action::New,
        KeyCode::F(2) if conns => Action::EditItem,
        KeyCode::Char('e') if conns => Action::EditItem,
        KeyCode::Char('n') if conns => Action::New,
        KeyCode::Delete if conns => Action::DeleteItem,
        _ => return None,
    })
}

/// A short hint shown in the footer.
pub struct Hint {
    pub keys: &'static str,
    pub label: &'static str,
}

const fn h(keys: &'static str, label: &'static str) -> Hint {
    Hint { keys, label }
}

/// Footer hints. `privileges` are the effective privileges on the open table:
/// editing hints only appear for what the user may actually do.
pub fn hints(mode: KeymapMode, ctx: Context, privileges: Privileges) -> Vec<Hint> {
    use KeymapMode::*;
    let mut out = match (mode, ctx) {
        (Standard, Context::Connections) => vec![
            h("↑↓", "select"),
            h("Enter", "connect"),
            h("n", "new"),
            h("e", "edit"),
            h("Del", "delete"),
            h("^K", "vim keys"),
            h("^Q", "quit"),
        ],
        (Vim, Context::Connections) => vec![
            h("j/k", "select"),
            h("Enter", "connect"),
            h("n", "new"),
            h("e", "edit"),
            h("dd", "delete"),
            h(":set novim", "standard keys"),
            h("q", "quit"),
        ],
        (Standard, Context::Sidebar) => {
            vec![
                h("↑↓", "move"),
                h("Enter", "open"),
                h("←→", "fold"),
                h("/", "filter"),
                h("Tab", "focus"),
                h("1-3", "tabs"),
            ]
        }
        (Vim, Context::Sidebar) => {
            vec![
                h("j/k", "move"),
                h("l/Enter", "open"),
                h("h", "fold"),
                h("/", "filter"),
                h("^l", "focus grid"),
                h("1-3", "tabs"),
            ]
        }
        (Standard, Context::Grid) => {
            vec![h("arrows", "move"), h("[ ]", "page"), h("Space", "view"), h("/", "filter"), h("^O", "sort")]
        }
        (Vim, Context::Grid) => {
            vec![h("hjkl", "move"), h("[ ]", "page"), h("v", "view"), h("/", "filter"), h("s", "sort")]
        }
        (_, Context::Structure) => vec![h("↑↓", "scroll"), h("1-3", "tabs"), h("Tab", "focus")],
    };
    if ctx == Context::Grid {
        let p = privileges;
        let (edit, delete, insert, undo, commit) = match mode {
            Standard => {
                (h("Enter", "edit"), h("Del", "delete"), h("Ins", "new row"), h("^Z", "undo"), h("^S", "commit"))
            }
            Vim => (h("i", "edit"), h("dd", "delete"), h("o", "new row"), h("u", "undo"), h(":w", "commit")),
        };
        if p.update {
            out.push(edit);
        }
        if p.delete {
            out.push(delete);
        }
        if p.insert {
            out.push(insert);
        }
        if p.can_write() {
            out.push(undo);
            out.push(commit);
        }
    }
    if ctx != Context::Connections {
        out.push(h("?", "help"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn vim_sequences() {
        let mut km = Keymap::default();
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('g')), None);
        assert_eq!(km.pending(), Some('g'));
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('g')), Some(Action::Top));
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('d')), None);
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('d')), Some(Action::DeleteRow));
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('j')), Some(Action::Down));
        // An unrelated key cancels a pending prefix.
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('d')), None);
        assert_eq!(km.resolve(KeymapMode::Vim, Context::Grid, key('x')), None);
        assert_eq!(km.pending(), None);
    }

    #[test]
    fn standard_ignores_vim_letters() {
        let mut km = Keymap::default();
        assert_eq!(km.resolve(KeymapMode::Standard, Context::Grid, key('j')), None);
        assert_eq!(
            km.resolve(KeymapMode::Standard, Context::Grid, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(Action::Down)
        );
        assert_eq!(
            km.resolve(KeymapMode::Standard, Context::Grid, KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            Some(Action::DeleteRow)
        );
    }

    #[test]
    fn arrows_work_in_vim_mode() {
        let mut km = Keymap::default();
        assert_eq!(
            km.resolve(KeymapMode::Vim, Context::Sidebar, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(Action::Up)
        );
    }

    #[test]
    fn hints_hide_editing_for_read_only() {
        let ro = hints(KeymapMode::Vim, Context::Grid, Privileges::READ);
        assert!(!ro.iter().any(|h| matches!(h.label, "edit" | "delete" | "new row" | "commit")));
        let update_only = Privileges { update: true, ..Privileges::READ };
        let labels: Vec<_> = hints(KeymapMode::Vim, Context::Grid, update_only).iter().map(|h| h.label).collect();
        assert!(labels.contains(&"edit") && labels.contains(&"commit"));
        assert!(!labels.contains(&"delete") && !labels.contains(&"new row"));
    }
}
