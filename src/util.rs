//! Small helpers shared across the UI.
//!
//! Text that comes from a database or a server (names, values, error
//! messages, the server version…) is drawn through [`display_safe`] or one
//! of the helpers built on it. Characters that would not show as themselves
//! are made visible instead: control characters, which the terminal library
//! drops without a trace, and invisible format characters (zero-width
//! spaces, bidi overrides…), which make different names look the same or
//! reorder the text around them.

use std::fmt::Write as _;
use std::ops::ControlFlow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Zero-width characters (combining marks) shown in a row; the rest of a
/// longer run is counted instead. A value can stack thousands of accents
/// on one letter, which the terminal would get as one huge cell.
const MAX_MARKS: usize = 8;

/// Tab stops of text shown on several lines.
const TAB: usize = 4;

/// Re-indents a JSON document; `None` when the text is not valid JSON.
///
/// Only whitespace changes: numbers, keys (duplicates too), their order and
/// escapes are kept as written, so what is shown is what is stored (parsing
/// into `serde_json::Value` would round large numbers and drop duplicates).
pub fn pretty_json(text: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let mut depth = 0usize;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    let (mut in_string, mut escaped) = (false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
                // Empty: `{}` / `[]`.
                if let Some(close) = chars.next_if(|c| matches!(c, '}' | ']')) {
                    out.push(close);
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            // Whitespace between tokens (JSON allows no other).
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
}

/// Whether `c` does not show as itself: control characters (C0, DEL, C1),
/// and format characters that are invisible or reorder the text around
/// them (zero-width spaces and joiners, bidi marks, embeddings, overrides
/// and isolates, the byte order mark, tag characters, blank fillers…).
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// Whether `s` holds hidden characters (see [`is_hidden`]) other than line
/// breaks and tabs, which are always shown as such.
pub fn has_hidden(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let line_break = c == '\n' || (c == '\r' && chars.next_if_eq(&'\n').is_some());
        if is_hidden(c) && !line_break && c != '\t' {
            return true;
        }
    }
    false
}

/// Writes how hidden character `c` is shown: C0 controls and DEL as their
/// "control pictures" (`␛` for ESC, `␀` for NUL…), anything else as its
/// code point, such as `⟨U+202E⟩`.
pub fn push_escaped(out: &mut String, c: char) {
    match c as u32 {
        n @ 0..=0x1f => out.push(char::from_u32(0x2400 + n).unwrap_or('\u{FFFD}')),
        0x7f => out.push('␡'),
        n => {
            let _ = write!(out, "⟨U+{n:04X}⟩");
        }
    }
}

/// Goes through the display form of `s`, piece by piece, until `visit`
/// breaks: the one place where hidden characters are made visible. Each
/// piece comes with its width in columns.
///
/// With `lines`, line breaks are kept (`\r\n` as `\n`) and tabs become
/// spaces; otherwise all is on one line, with line breaks shown as `↵` and
/// tabs as `⇥`.
fn walk(s: &str, lines: bool, mut visit: impl FnMut(&str, usize) -> ControlFlow<()>) {
    let mut chars = s.chars().peekable();
    let mut piece = String::new();
    let (mut column, mut marks) = (0, 0);
    while let Some(c) = chars.next() {
        piece.clear();
        let mark = !is_hidden(c) && c.width() == Some(0);
        marks = if mark { marks + 1 } else { 0 };
        let width = if c == '\n' || (c == '\r' && chars.next_if_eq(&'\n').is_some()) {
            if lines {
                piece.push('\n');
                column = 0;
                0
            } else {
                piece.push('↵');
                1
            }
        } else if c == '\t' {
            if lines {
                let n = TAB - column % TAB;
                piece.extend(std::iter::repeat_n(' ', n));
                n
            } else {
                piece.push('⇥');
                1
            }
        } else if is_hidden(c) {
            push_escaped(&mut piece, c);
            piece.width()
        } else if mark && marks > MAX_MARKS {
            let mut more = 1;
            while chars.next_if(|&c| !is_hidden(c) && c.width() == Some(0)).is_some() {
                more += 1;
            }
            let _ = write!(piece, "⟨+{more} marks⟩");
            piece.width()
        } else {
            piece.push(c);
            c.width().unwrap_or(0)
        };
        column += width;
        if visit(&piece, width).is_break() {
            return;
        }
    }
}

/// `s` made safe to display on one line: hidden characters are shown as
/// symbols (see [`push_escaped`]), line breaks as `↵` and tabs as `⇥`.
pub fn display_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    walk(s, false, |piece, _| {
        out.push_str(piece);
        ControlFlow::Continue(())
    });
    out
}

/// `s` made safe to display on several lines: as [`display_safe`], but
/// line breaks are kept and tabs become spaces.
pub fn display_safe_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    walk(s, true, |piece, _| {
        out.push_str(piece);
        ControlFlow::Continue(())
    });
    out
}

/// A name from the database (schema, table, column, index…) made safe to
/// display, behind a `⚠` when it holds hidden characters: `accounts` and
/// `accounts\u{200B}` look the same otherwise, and are not the same table.
pub fn ident(s: &str) -> String {
    let safe = display_safe(s);
    if has_hidden(s) { format!("⚠ {safe}") } else { safe }
}

/// `s` made safe to display on one line (see [`display_safe`]) and cut to
/// `width` columns, with an ellipsis when cut. Only as much of `s` is read
/// as is shown, however long `s` is.
pub fn fit(s: &str, width: usize) -> String {
    fit_cut(s, width, false)
}

/// As [`fit`]; `cut` adds the ellipsis even when all of `s` fits, for text
/// that is itself the start of a longer value.
pub fn fit_cut(s: &str, width: usize, cut: bool) -> String {
    let mut out = String::new();
    if width == 0 {
        return out;
    }
    // `keep`: how much of `out` leaves room for the ellipsis.
    let (mut used, mut keep, mut over) = (0, 0, false);
    walk(s, false, |piece, w| {
        if used + w > width {
            over = true;
            return ControlFlow::Break(());
        }
        out.push_str(piece);
        used += w;
        if used < width {
            keep = out.len();
        }
        ControlFlow::Continue(())
    });
    if over || cut {
        out.truncate(keep);
        out.push('…');
    }
    out
}

/// Width of `s` shown on one line (see [`display_safe`]), counted only up
/// to a little past `max` columns.
pub fn width_up_to(s: &str, max: usize) -> usize {
    let mut used = 0;
    walk(s, false, |_, w| {
        used += w;
        if used > max { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    });
    used
}

/// Splits `text` (already safe to display, see [`display_safe_lines`]) into
/// lines at most `width` columns wide, breaking after a space where there
/// is one. Done once for a large value, rather than on every frame.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        let mut used = 0;
        // Where the line can break: after the last space, and the width up to it.
        let mut space: Option<(usize, usize)> = None;
        for c in line.chars() {
            let w = c.width().unwrap_or(0);
            while used + w > width && used > 0 {
                match space.take().filter(|&(at, _)| at < current.len()) {
                    Some((at, before)) => {
                        let rest = current.split_off(at);
                        out.push(std::mem::replace(&mut current, rest));
                        used -= before;
                    }
                    None => {
                        out.push(std::mem::take(&mut current));
                        used = 0;
                    }
                }
            }
            current.push(c);
            used += w;
            if c == ' ' {
                space = Some((current.len(), used));
            }
        }
        out.push(current);
    }
    out
}

/// The start of `s`, at most `max` bytes long (cut at a character
/// boundary), and whether anything was cut.
pub fn clip(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max { (s, false) } else { (&s[..s.floor_char_boundary(max)], true) }
}

/// A number of bytes for messages: `512 B`, `256 KiB`, `1.5 MiB`.
pub fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let (mut value, mut unit) = (bytes as f64 / 1024.0, 0);
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let number = format!("{value:.1}");
    format!("{} {}", number.strip_suffix(".0").unwrap_or(&number), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello world", 6), "hello…");
        assert_eq!(fit("日本語テキスト", 5), "日本…");
        assert_eq!(fit("a\x1bb", 10), "a␛b");
        assert_eq!(fit("a\u{202E}bcdef", 5), "a…");
        assert_eq!(fit("a\u{202E}bcdef", 20), "a⟨U+202E⟩bcdef");
        // The start of a longer value always says so.
        assert_eq!(fit_cut("hello", 10, true), "hello…");
        assert_eq!(fit_cut("hello", 5, true), "hell…");
        assert_eq!(fit("anything", 0), "");
        assert_eq!(width_up_to(&"x".repeat(1000), 40), 41);
        assert_eq!(width_up_to("a\u{200B}", 40), 9);
    }

    /// Audit L5: characters that would not show as themselves are made
    /// visible, never dropped; bidi controls cannot reorder the screen.
    #[test]
    fn hidden_characters_are_shown() {
        assert_eq!(display_safe("a\x1b[2Jb\x07"), "a␛[2Jb␇");
        assert_eq!(display_safe("x\u{9b}31m"), "x⟨U+009B⟩31m");
        assert_eq!(display_safe("accounts\u{200B}"), "accounts⟨U+200B⟩");
        assert_eq!(display_safe("evil\u{202E}gnp.exe"), "evil⟨U+202E⟩gnp.exe");
        assert_eq!(display_safe("\u{2066}x\u{2069}\u{FEFF}\u{E0041}\x7f"), "⟨U+2066⟩x⟨U+2069⟩⟨U+FEFF⟩⟨U+E0041⟩␡");
        assert_eq!(display_safe("a\nb\r\nc\rd\te"), "a↵b↵c␍d⇥e");
        assert_eq!(display_safe_lines("a\nb\r\nc\rd\te\n\tf"), "a\nb\nc␍d e\n    f");
        // Ordinary text, accents and emoji are left alone.
        for text in ["café", "cafe\u{301}", "日本語", "Ünïcödé ✓ ⚠", "👍", ""] {
            assert_eq!(display_safe(text), text);
        }
        let all: String = (0..=0x10FFFF).filter_map(char::from_u32).filter(|c| is_hidden(*c)).collect();
        let shown = display_safe(&all);
        assert!(!shown.chars().any(is_hidden), "{shown}");
        assert_eq!(display_safe(&shown), shown, "showing it again changes nothing");
    }

    /// Audit L5: names that differ only by hidden characters look different,
    /// and are flagged.
    #[test]
    fn lookalike_names_are_flagged() {
        let names = ["accounts", "accounts\u{200B}", "accounts\x01", "acc\u{00AD}ounts", "accounts\u{2060}"];
        let shown: Vec<String> = names.iter().map(|n| ident(n)).collect();
        assert_eq!(shown[..3], ["accounts", "⚠ accounts⟨U+200B⟩", "⚠ accounts␁"]);
        assert_eq!(shown.iter().collect::<std::collections::HashSet<_>>().len(), names.len());
        // Line breaks and tabs are always shown as such.
        assert!(!has_hidden("two\nlines\r\nand\ta tab"));
        assert!(has_hidden("a lone\rcarriage return") && has_hidden("\u{202E}"));
    }

    /// A run of combining marks on one letter is shown up to a point, then
    /// counted: the terminal would otherwise get it as one huge cell.
    #[test]
    fn stacked_marks_are_counted() {
        let zalgo = format!("a{}b", "\u{301}".repeat(100_000));
        let marks = "\u{301}".repeat(MAX_MARKS);
        assert_eq!(display_safe(&zalgo), format!("a{marks}⟨+{} marks⟩b", 100_000 - MAX_MARKS));
        assert_eq!(display_safe("e\u{301}\u{302}"), "e\u{301}\u{302}");
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("hello world foo", 11), ["hello ", "world foo"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 4), ["a", "", "b"]);
        assert_eq!(wrap("日本語", 4), ["日本", "語"]);
        assert_eq!(wrap("", 5), [""]);
    }

    #[test]
    fn sizes_and_clips() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(256 * 1024), "256 KiB");
        assert_eq!(size(1536 * 1024), "1.5 MiB");
        assert_eq!(clip("héllo", 2), ("h", true));
        assert_eq!(clip("héllo", 6), ("héllo", false));
    }

    #[test]
    fn json() {
        assert_eq!(pretty_json("{\"a\":1}").unwrap(), "{\n  \"a\": 1\n}");
        assert_eq!(
            pretty_json(" { \"a\" : [1, {}, [ ]], \"b\" :\n\"x, \\\"y\\\": [z]\" } ").unwrap(),
            "{\n  \"a\": [\n    1,\n    {},\n    []\n  ],\n  \"b\": \"x, \\\"y\\\": [z]\"\n}"
        );
        assert!(pretty_json("{nope").is_none());
        assert!(pretty_json("[1] x").is_none());
    }

    /// Audit (data fidelity): the viewer shows what is stored. Parsing the
    /// document into values would round numbers and drop duplicate keys.
    #[test]
    fn json_is_only_reindented() {
        let stored = r#"{"b":1,"a":12345678901234567890123,"c":0.1000000000000000055511151231257827,"b":2,"e":1E400}"#;
        let shown = pretty_json(stored).unwrap();
        let tokens = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(tokens(&shown), tokens(stored));
        assert!(shown.contains("\"a\": 12345678901234567890123,") && shown.ends_with("\"e\": 1E400\n}"), "{shown}");
    }
}
