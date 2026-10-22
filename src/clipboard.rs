//! Clipboard support through the OSC 52 escape sequence, which most modern
//! terminals (and tmux / SSH sessions) understand without extra libraries.

use std::io::Write;

use base64::Engine;

pub fn copy(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let seq = if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    };
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}
