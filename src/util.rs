//! Small helpers shared across the UI.

/// Re-indents a JSON document; `None` when the text is not valid JSON.
pub fn pretty_json(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    serde_json::to_string_pretty(&value).ok()
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
        assert!(pretty_json("{nope").is_none());
    }
}
