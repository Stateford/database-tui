//! Colour palette (Tokyo Night inspired) and shared styles.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);
pub const BG_DARK: Color = Color::Rgb(0x16, 0x16, 0x1e);
pub const SURFACE: Color = Color::Rgb(0x24, 0x28, 0x3b);
pub const HIGHLIGHT: Color = Color::Rgb(0x29, 0x2e, 0x42);
pub const SELECTION: Color = Color::Rgb(0x33, 0x46, 0x7c);
pub const BORDER: Color = Color::Rgb(0x3b, 0x42, 0x61);
pub const FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
pub const FG_DIM: Color = Color::Rgb(0xa9, 0xb1, 0xd6);
pub const MUTED: Color = Color::Rgb(0x56, 0x5f, 0x89);
pub const BLUE: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
pub const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
pub const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
pub const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
pub const ORANGE: Color = Color::Rgb(0xff, 0x9e, 0x64);
pub const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
pub const MAGENTA: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
pub const TEAL: Color = Color::Rgb(0x1a, 0xbc, 0x9c);

pub const EDITED_BG: Color = Color::Rgb(0x3d, 0x35, 0x1f);
pub const DELETED_BG: Color = Color::Rgb(0x3f, 0x22, 0x2b);
pub const INSERTED_BG: Color = Color::Rgb(0x23, 0x3a, 0x2b);

pub fn base() -> Style {
    Style::default().fg(FG).bg(BG)
}

pub fn dim() -> Style {
    Style::default().fg(MUTED)
}

pub fn border(focused: bool) -> Style {
    Style::default().fg(if focused { BLUE } else { BORDER })
}

pub fn title(focused: bool) -> Style {
    let s = Style::default().fg(if focused { BLUE } else { FG_DIM });
    if focused { s.add_modifier(Modifier::BOLD) } else { s }
}

/// A rounded "pill" label: coloured background, dark text.
pub fn pill(bg: Color) -> Style {
    Style::default().fg(BG_DARK).bg(bg).add_modifier(Modifier::BOLD)
}

pub fn key() -> Style {
    Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
}

pub fn null() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
}

pub fn driver_color(driver: crate::db::Driver) -> Color {
    match driver {
        crate::db::Driver::Postgres => BLUE,
        crate::db::Driver::MySql => ORANGE,
        crate::db::Driver::Sqlite => TEAL,
    }
}
