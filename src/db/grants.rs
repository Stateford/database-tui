//! Parser for MySQL/MariaDB `SHOW GRANTS` output, used to work out which
//! tables the current account may modify.

use super::Privileges;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PrivSet {
    select: bool,
    insert: bool,
    update: bool,
    delete: bool,
    superuser: bool,
}

impl PrivSet {
    fn merge(&mut self, other: PrivSet) {
        self.select |= other.select;
        self.insert |= other.insert;
        self.update |= other.update;
        self.delete |= other.delete;
        self.superuser |= other.superuser;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Global,
    Database(String),
    Table(String, String),
}

#[derive(Debug, Default, Clone)]
pub struct Grants {
    global: PrivSet,
    databases: Vec<(String, PrivSet)>,
    tables: Vec<(String, String, PrivSet)>,
}

impl Grants {
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Grants {
        let mut grants = Grants::default();
        for line in lines {
            if let Some((set, scope)) = parse_line(line) {
                match scope {
                    Scope::Global => grants.global.merge(set),
                    Scope::Database(db) => grants.databases.push((db, set)),
                    Scope::Table(db, t) => grants.tables.push((db, t, set)),
                }
            }
        }
        grants
    }

    pub fn is_superuser(&self) -> bool {
        self.global.superuser
    }

    pub fn privileges_for(&self, db: &str, table: &str) -> Privileges {
        let mut set = self.global;
        for (pattern, s) in &self.databases {
            if like_match(pattern, db) {
                set.merge(*s);
            }
        }
        for (d, t, s) in &self.tables {
            if d.eq_ignore_ascii_case(db) && t.eq_ignore_ascii_case(table) {
                set.merge(*s);
            }
        }
        Privileges { select: set.select, insert: set.insert, update: set.update, delete: set.delete }
    }
}

/// Parses `GRANT <privs> ON <object> TO <account> ...`. Role grants and
/// routine/proxy grants return `None`.
fn parse_line(line: &str) -> Option<(PrivSet, Scope)> {
    let line = line.trim();
    let upper = line.to_ascii_uppercase();
    if !upper.starts_with("GRANT ") {
        return None;
    }
    let on = find_keyword(&upper, " ON ")?;
    let to = find_keyword(&upper[on..], " TO ")? + on;
    let privs = &line[6..on];
    let object = line[on + 4..to].trim();
    let object_upper = object.to_ascii_uppercase();
    if object_upper.starts_with("FUNCTION ") || object_upper.starts_with("PROCEDURE ") || object.contains('@') {
        return None;
    }
    let scope = parse_object(object)?;
    let mut set = PrivSet::default();
    for item in split_top_level(privs) {
        if item.contains('(') {
            // Column-level privilege: not enough to edit whole rows.
            continue;
        }
        match item.trim().to_ascii_uppercase().as_str() {
            "ALL" | "ALL PRIVILEGES" => {
                set.select = true;
                set.insert = true;
                set.update = true;
                set.delete = true;
                if scope == Scope::Global {
                    set.superuser = true;
                }
            }
            "SELECT" => set.select = true,
            "INSERT" => set.insert = true,
            "UPDATE" => set.update = true,
            "DELETE" => set.delete = true,
            "SUPER" => set.superuser = true,
            _ => {}
        }
    }
    Some((set, scope))
}

/// Finds a keyword outside of quotes/backticks.
fn find_keyword(upper: &str, keyword: &str) -> Option<usize> {
    let bytes = upper.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'`' || b == b'\'' || b == b'"' => quote = Some(b),
            None if upper[i..].starts_with(keyword) => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

fn split_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_object(object: &str) -> Option<Scope> {
    let object = object.strip_prefix("TABLE ").unwrap_or(object).trim();
    let (db, rest) = read_ident(object)?;
    let rest = rest.strip_prefix('.')?;
    let (table, _) = read_ident(rest)?;
    Some(match (db.as_str(), table.as_str()) {
        ("*", "*") => Scope::Global,
        (_, "*") => Scope::Database(db),
        _ => Scope::Table(unescape_wildcards(&db), table),
    })
}

fn read_ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let first = s.chars().next()?;
    if first == '*' {
        return Some(("*".into(), &s[1..]));
    }
    if first == '`' || first == '\'' || first == '"' {
        let mut out = String::new();
        let mut chars = s[1..].char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == first {
                if matches!(chars.peek(), Some((_, n)) if *n == first) {
                    out.push(c);
                    chars.next();
                    continue;
                }
                return Some((out, &s[i + 2..]));
            }
            out.push(c);
        }
        return None;
    }
    let end = s.find(|c: char| c == '.' || c.is_whitespace()).unwrap_or(s.len());
    Some((s[..end].to_string(), &s[end..]))
}

fn unescape_wildcards(s: &str) -> String {
    s.replace("\\_", "_").replace("\\%", "%")
}

/// MySQL database-level grants use LIKE patterns (`%`, `_`, `\` escapes).
fn like_match(pattern: &str, value: &str) -> bool {
    fn rec(p: &[char], v: &[char]) -> bool {
        match p.first() {
            None => v.is_empty(),
            Some('%') => (0..=v.len()).any(|i| rec(&p[1..], &v[i..])),
            Some('_') => !v.is_empty() && rec(&p[1..], &v[1..]),
            Some('\\') if p.len() > 1 => !v.is_empty() && p[1].eq_ignore_ascii_case(&v[0]) && rec(&p[2..], &v[1..]),
            Some(c) => !v.is_empty() && c.eq_ignore_ascii_case(&v[0]) && rec(&p[1..], &v[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    rec(&p, &v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_all_is_superuser() {
        let g = Grants::parse(["GRANT ALL PRIVILEGES ON *.* TO `root`@`localhost` WITH GRANT OPTION"]);
        assert!(g.is_superuser());
        assert_eq!(g.privileges_for("shop", "users"), Privileges::ALL);
    }

    #[test]
    fn read_only_user() {
        let g = Grants::parse([
            "GRANT USAGE ON *.* TO `viewer`@`%`",
            "GRANT SELECT, SHOW VIEW ON `shop`.* TO `viewer`@`%`",
        ]);
        assert!(!g.is_superuser());
        assert_eq!(g.privileges_for("shop", "users"), Privileges::READ);
        assert_eq!(g.privileges_for("other", "users"), Privileges::default());
    }

    #[test]
    fn table_and_column_level() {
        let g = Grants::parse([
            "GRANT SELECT ON `shop`.* TO 'app'@'%'",
            "GRANT INSERT, UPDATE (`name`), DELETE ON `shop`.`orders` TO 'app'@'%'",
        ]);
        let p = g.privileges_for("shop", "orders");
        assert!(p.select && p.insert && p.delete && !p.update);
        assert_eq!(g.privileges_for("shop", "users"), Privileges::READ);
    }

    #[test]
    fn wildcard_databases() {
        let g = Grants::parse(["GRANT SELECT, UPDATE ON `app\\_%`.* TO `dev`@`%`"]);
        assert!(g.privileges_for("app_prod", "t").update);
        assert!(!g.privileges_for("appxprod", "t").update);
    }

    #[test]
    fn ignores_roles_and_routines() {
        let g = Grants::parse([
            "GRANT `writer`@`%` TO `bob`@`%`",
            "GRANT EXECUTE ON PROCEDURE `shop`.`p` TO `bob`@`%`",
            "GRANT PROXY ON ''@'' TO 'root'@'localhost' WITH GRANT OPTION",
        ]);
        assert_eq!(g.privileges_for("shop", "t"), Privileges::default());
    }
}
