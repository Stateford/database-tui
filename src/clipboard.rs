//! Clipboard support through the OSC 52 escape sequence, which most modern
//! terminals (and tmux / SSH sessions) understand without extra libraries.

use std::borrow::Cow;

use base64::Engine;

/// Most bytes put on the clipboard at once: the terminal receives them
/// base64-encoded, in a single escape sequence.
pub const COPY_LIMIT: usize = 1024 * 1024;

/// How the text put on the clipboard differs from the text copied.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Copied {
    /// Control characters replaced by visible symbols.
    pub escaped: usize,
    /// Only the first [`COPY_LIMIT`] bytes were copied.
    pub cut: bool,
}

/// Puts `text` on the system clipboard, made safe to paste first (see
/// [`clipboard_text`]).
pub fn copy(text: &str) -> Copied {
    let (text, copied) = clipboard_text(text);
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    };
    send(seq);
    copied
}

/// `text` as it goes on the clipboard: at most [`COPY_LIMIT`] bytes, with
/// control characters other than tabs and line breaks (`\n`, `\r\n`)
/// replaced by visible symbols (`␛` for ESC, `⟨U+009B⟩` for C1 CSI…).
///
/// A value is data, and pasting it must not act as keystrokes. In a
/// terminal, `ESC [201~` ends a bracketed paste early, so whatever follows
/// it is typed rather than pasted (`ESC [15~` is F5, which runs the SQL
/// editor), and DEL erases what was typed before it.
pub fn clipboard_text(text: &str) -> (Cow<'_, str>, Copied) {
    let (text, cut) = crate::util::clip(text, COPY_LIMIT);
    let mut copied = Copied { escaped: 0, cut };
    let unsafe_char = |(i, c): (usize, char)| c.is_control() && c != '\n' && c != '\t' && !is_crlf(text, i, c);
    if !text.char_indices().any(unsafe_char) {
        return (Cow::Borrowed(text), copied);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for (i, c) in text.char_indices() {
        if unsafe_char((i, c)) {
            crate::util::push_escaped(&mut out, c);
            copied.escaped += 1;
        } else {
            out.push(c);
        }
    }
    (Cow::Owned(out), copied)
}

/// Whether `c`, at byte `i` of `text`, is the `\r` of a `\r\n`.
fn is_crlf(text: &str, i: usize, c: char) -> bool {
    c == '\r' && text.as_bytes().get(i + 1) == Some(&b'\n')
}

#[cfg(not(test))]
fn send(seq: String) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
thread_local! {
    static SENT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Tests keep the sequence instead of writing it to the terminal running
/// them (see [`sent`]).
#[cfg(test)]
fn send(seq: String) {
    SENT.with(|sent| *sent.borrow_mut() = Some(seq));
}

/// What the last copy on this thread put on the clipboard, decoded. Tests
/// may run in tmux, which gets the sequence wrapped.
#[cfg(test)]
pub fn sent() -> Option<String> {
    let seq = SENT.with(|sent| sent.borrow().clone())?;
    let seq = match seq.strip_prefix("\x1bPtmux;\x1b") {
        Some(inner) => inner.strip_suffix("\x1b\\")?.to_string(),
        None => seq,
    };
    let encoded = seq.strip_prefix("\x1b]52;c;")?.strip_suffix('\x07')?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit L4: a value cannot end a bracketed paste and type the rest.
    #[test]
    fn control_characters_cannot_act_as_keys() {
        let value = "x\x1b[201~'; DROP TABLE t; --\x1b[15~";
        let (text, copied) = clipboard_text(value);
        assert_eq!(text, "x␛[201~'; DROP TABLE t; --␛[15~");
        assert_eq!(copied, Copied { escaped: 2, cut: false });
        let (text, copied) = clipboard_text("a\u{9b}201~b\x7f\x00c\rd");
        assert_eq!(text, "a⟨U+009B⟩201~b␡␀c␍d");
        assert_eq!(copied.escaped, 4);
        assert!(!text.chars().any(|c| c.is_control()));
    }

    #[test]
    fn ordinary_text_is_copied_as_it_is() {
        for value in ["plain", "tabs\tand\nlines\r\nCRLF\r\n", "日本語 ✓ \u{202E}bidi stays", ""] {
            assert_eq!(clipboard_text(value), (Cow::Borrowed(value), Copied::default()));
        }
    }

    #[test]
    fn copies_are_limited() {
        let value = format!("{}é", "x".repeat(COPY_LIMIT - 1));
        let (text, copied) = clipboard_text(&value);
        assert_eq!((text.len(), copied), (COPY_LIMIT - 1, Copied { escaped: 0, cut: true }));
        copy("a\x1bb");
        assert_eq!(sent().as_deref(), Some("a␛b"));
    }
}
