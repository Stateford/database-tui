//! Key binding reference shown by `?` / F1.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::*;
use crate::config::KeymapMode;

type Section = (&'static str, &'static [(&'static str, &'static str)]);

const STANDARD: &[Section] = &[
    (
        "General",
        &[
            ("Tab / Shift-Tab", "cycle focus: explorer → data → (results)"),
            ("1  2  3", "Data / Structure / SQL tab"),
            ("F1  ?", "this help"),
            ("Ctrl-K", "switch to vim keys"),
            ("Ctrl-Q", "quit"),
            ("Esc", "back / disconnect from the explorer"),
        ],
    ),
    (
        "Explorer",
        &[
            ("↑ ↓  PgUp PgDn", "move"),
            ("Enter  →", "open table / expand schema"),
            ("←", "collapse"),
            ("/  Ctrl-F", "filter tables"),
            ("F5  Ctrl-R", "reload schema"),
        ],
    ),
    (
        "Data grid",
        &[
            ("arrows  PgUp PgDn", "move"),
            ("Home End  Ctrl-Home Ctrl-End", "first/last column, first/last row"),
            ("[  ]  Alt-← Alt-→", "previous / next page"),
            ("Space  F3", "view full value (JSON re-indented)"),
            ("Ctrl-C", "copy cell (system clipboard via OSC 52, controls as ␛…)"),
            ("/  Ctrl-F", "filter with a WHERE condition"),
            ("Ctrl-O  F6", "sort by column (asc → desc → off)"),
            ("F5  Ctrl-R", "reload"),
        ],
    ),
    (
        "Editing (needs write permission)",
        &[
            ("Enter  F2", "edit cell"),
            ("Backspace", "set cell to NULL"),
            ("Ctrl-V", "paste copied value into cell"),
            ("Insert  Ctrl-N", "add a new row"),
            ("Delete", "mark / unmark row for deletion"),
            ("Ctrl-Z", "undo last staged change"),
            ("Ctrl-S", "review & commit staged changes"),
        ],
    ),
    (
        "Value viewer",
        &[
            ("↑ ↓  PgUp PgDn", "scroll"),
            ("Home  End", "top / end"),
            ("y  c", "copy the value"),
            ("Esc  q  Space", "close"),
        ],
    ),
    (
        "Cell editor",
        &[
            ("Enter", "apply"),
            ("Alt-Enter", "new line"),
            ("Ctrl-N", "set NULL"),
            ("Ctrl-D", "use default (new rows)"),
            ("Esc", "cancel"),
        ],
    ),
    (
        "Commit preview",
        &[
            ("y  Enter", "commit"),
            ("!", "commit anyway on tables that cannot roll back (MyISAM…)"),
            ("Esc", "back to editing"),
        ],
    ),
    (
        "SQL editor",
        &[("F5  Ctrl-E", "run (selection only, if any)"), ("Tab", "move to results"), ("Esc", "leave editor")],
    ),
];

const VIM: &[Section] = &[
    (
        "General",
        &[
            ("Tab  Ctrl-h  Ctrl-l", "cycle focus / focus explorer / focus main"),
            ("1 2 3  gt gT", "switch tabs"),
            ("?", "this help"),
            (":", "command line (see below)"),
            ("q  Esc", "back / disconnect from the explorer"),
        ],
    ),
    (
        "Motions",
        &[
            ("h j k l", "move"),
            ("gg  G", "first / last row"),
            ("0 ^  $", "first / last column"),
            ("Ctrl-d Ctrl-u", "half page down / up"),
            ("Ctrl-f Ctrl-b", "page down / up"),
            ("[  ]", "previous / next result page"),
        ],
    ),
    (
        "Data grid",
        &[
            ("v  Space", "view full value"),
            ("yy", "yank (copy) cell, controls as ␛…"),
            ("/", "filter with a WHERE condition"),
            ("s", "sort by column"),
            ("R  Ctrl-r", "reload"),
        ],
    ),
    (
        "Editing (needs write permission)",
        &[
            ("i  a  Enter", "edit cell"),
            ("x", "set cell to NULL"),
            ("p", "paste yanked value"),
            ("o", "add a new row"),
            ("dd", "mark / unmark row for deletion"),
            ("u", "undo last staged change"),
            (":w  Ctrl-s", "review & commit staged changes"),
        ],
    ),
    (
        "Value viewer",
        &[("j k  Ctrl-d Ctrl-u", "scroll"), ("g  G", "top / end"), ("y  c", "copy the value"), ("Esc  q  v", "close")],
    ),
    (
        "Commit preview",
        &[
            ("y  Enter", "commit"),
            ("!", "commit anyway on tables that cannot roll back (MyISAM…)"),
            ("Esc", "back to editing"),
        ],
    ),
    (
        "SQL editor (modal)",
        &[
            ("i a I A o O", "insert mode;  Esc back to normal"),
            ("v V  y d c", "visual select, yank, delete, change"),
            ("dd yy cc p u Ctrl-r", "line ops, paste, undo, redo"),
            ("w b e 0 $ gg G", "motions"),
            ("F5  Ctrl-E", "run (selection only, if any)"),
        ],
    ),
    (
        "Commands",
        &[
            (":w", "commit staged changes"),
            (":e!", "discard staged changes"),
            (":q  :q!", "disconnect / quit (! discards changes)"),
            (":filter <cond>  :nofilter", "filter rows"),
            (":sort <col> [desc]", "sort rows"),
            (":<n>", "go to row n"),
            (":sql  :data  :structure", "switch tab"),
            (":set novim", "switch to standard keys"),
        ],
    ),
];

pub fn lines(mode: KeymapMode) -> Vec<Line<'static>> {
    let sections = match mode {
        KeymapMode::Standard => STANDARD,
        KeymapMode::Vim => VIM,
    };
    let mut out = Vec::new();
    for (title, keys) in sections {
        out.push(Line::styled(*title, Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)));
        for (k, d) in keys.iter() {
            out.push(Line::from(vec![
                Span::styled(format!("  {k:<30}"), key()),
                Span::styled(*d, Style::default().fg(FG)),
            ]));
        }
        out.push(Line::from(""));
    }
    out.push(Line::styled("Permissions", Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)));
    for (k, d, color) in [
        ("READ-ONLY", "you can browse and query, edits are disabled", YELLOW),
        ("READ-WRITE", "edits allowed where the table privileges permit", GREEN),
        ("SUPERUSER", "full access — be careful", RED),
    ] {
        out.push(Line::from(vec![
            Span::styled(format!("  {k:<30}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(d, Style::default().fg(FG)),
        ]));
    }
    out.push(Line::from(vec![
        Span::styled(format!("  {:<30}", "SEL INS UPD DEL"), Style::default().fg(GREEN)),
        Span::styled("privileges on the open table (crossed out = missing)", Style::default().fg(FG)),
    ]));
    out.push(Line::from(""));
    out.push(Line::styled("Symbols in data and names", Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)));
    for (k, d) in [
        ("↵  ⇥", "line break, tab (on one line)"),
        ("␛ ␀ ␇ …", "control character (ESC, NUL, BEL…)"),
        ("⟨U+202E⟩", "invisible or bidi character, by code point"),
        ("⚠ name", "a name holding such characters: not the name it looks like"),
        ("…", "cut to fit; for a large value, only its start was loaded"),
    ] {
        out.push(Line::from(vec![Span::styled(format!("  {k:<30}"), key()), Span::styled(d, Style::default().fg(FG))]));
    }
    out
}
