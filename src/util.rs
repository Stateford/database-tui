//! Small helpers shared across the UI.

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

/// Truncates `s` to `width` terminal columns, adding an ellipsis when cut.
pub fn truncate(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Single-line representation of a cell value.
pub fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' => '↵',
            '\r' => ' ',
            '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語テキスト", 5), "日本…");
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
