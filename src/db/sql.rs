//! SQL generation and inspection helpers. Everything here is pure so it can be
//! unit tested without a database.

use super::{CellKind, ColumnInfo, Driver, PageRequest, TableRef};

/// A parameterised statement. Values typed by the user are sent as text (or
/// NULL) and converted by the database to the column type; the keys that
/// identify a row are sent as what they were read as (see [`Value`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Option<Value>>,
    /// The statement must affect exactly one row (updates/deletes by key).
    pub expect_one: bool,
}

/// A value bound to a statement parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Text, converted by the database to the column type.
    Text(String),
    /// A SQLite INTEGER or REAL, bound as one: in a column without type
    /// affinity, integer 1 and text '1' are different keys.
    Integer(i64),
    Real(f64),
    /// Bytes (SQLite BLOB, MySQL binary strings), never their `0x…` text.
    Bytes(Vec<u8>),
}

impl Value {
    /// The value that a cell shown as `text` was read as, so that a key
    /// matches exactly the row it was read from. `None` when the text does
    /// not give the value back exactly.
    pub fn from_cell(text: &str, kind: CellKind) -> Option<Value> {
        match kind {
            CellKind::Text => Some(Value::Text(text.to_string())),
            CellKind::Integer => text.parse().ok().map(Value::Integer),
            // Shown as Rust's shortest form that reads back as the same f64.
            CellKind::Real => text.parse().ok().map(Value::Real),
            CellKind::Bytes => parse_hex(text).map(Value::Bytes),
            CellKind::Inexact | CellKind::Truncated(_) => None,
        }
    }

    fn literal(&self) -> String {
        match self {
            Value::Text(s) => quote_literal(s),
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => format!("{f:?}"),
            Value::Bytes(b) => format!("x'{}'", b.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s)
    }
}

/// Bytes from `0x` + hex, as [`super::hex_bytes`] shows them.
fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("0x")?;
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

impl Statement {
    /// Human readable form used in the commit preview.
    pub fn preview(&self) -> String {
        if self.params.is_empty() {
            return format!("{};", self.sql);
        }
        let params: Vec<String> = self
            .params
            .iter()
            .map(|p| match p {
                Some(v) => v.literal(),
                None => "NULL".to_string(),
            })
            .collect();
        format!("{};  -- [{}]", self.sql, params.join(", "))
    }
}

pub fn quote_ident(driver: Driver, ident: &str) -> String {
    match driver {
        Driver::MySql => format!("`{}`", ident.replace('`', "``")),
        Driver::Postgres | Driver::Sqlite => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn qualified(driver: Driver, table: &TableRef) -> String {
    if driver == Driver::Sqlite && (table.schema.is_empty() || table.schema == "main") {
        return quote_ident(driver, &table.name);
    }
    format!("{}.{}", quote_ident(driver, &table.schema), quote_ident(driver, &table.name))
}

fn placeholder(driver: Driver, n: usize, column: &ColumnInfo, value: Option<&Value>) -> String {
    match driver {
        // Text is cast to the column's base type (never to e.g.
        // `varchar(40)`, which would truncate long values).
        Driver::Postgres if matches!(value, None | Some(Value::Text(_))) => {
            let ty = if column.base_type.is_empty() { &column.data_type } else { &column.base_type };
            format!("CAST(${n} AS {ty})")
        }
        // Other values are bound with their own type.
        Driver::Postgres => format!("${n}"),
        Driver::MySql | Driver::Sqlite => "?".to_string(),
    }
}

fn where_clause(filter: Option<&str>) -> String {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!(" WHERE ({f})"),
        None => String::new(),
    }
}

pub fn select_page(driver: Driver, table: &TableRef, req: &PageRequest) -> String {
    let mut sql = format!("SELECT * FROM {}{}", qualified(driver, table), where_clause(req.filter.as_deref()));
    if let Some((col, asc)) = &req.order_by {
        sql.push_str(&format!(" ORDER BY {} {}", quote_ident(driver, col), if *asc { "ASC" } else { "DESC" }));
    } else if !req.default_order.is_empty() {
        let cols: Vec<String> = req.default_order.iter().map(|c| quote_ident(driver, c)).collect();
        sql.push_str(&format!(" ORDER BY {}", cols.join(", ")));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", req.limit, req.offset));
    sql
}

pub fn count_rows(driver: Driver, table: &TableRef, filter: Option<&str>) -> String {
    format!("SELECT COUNT(*) FROM {}{}", qualified(driver, table), where_clause(filter))
}

fn key_conditions(driver: Driver, key: &[(&ColumnInfo, Option<Value>)], params: &mut Vec<Option<Value>>) -> String {
    key.iter()
        .map(|(col, value)| match value {
            None => format!("{} IS NULL", quote_ident(driver, &col.name)),
            Some(v) => {
                params.push(Some(v.clone()));
                format!("{} = {}", quote_ident(driver, &col.name), placeholder(driver, params.len(), col, Some(v)))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `key`: the values that identify the row, as read (see [`Value::from_cell`]).
pub fn update_row(
    driver: Driver,
    table: &TableRef,
    key: &[(&ColumnInfo, Option<Value>)],
    set: &[(&ColumnInfo, Option<String>)],
) -> Statement {
    let mut params = Vec::new();
    let assignments = set
        .iter()
        .map(|(col, value)| {
            params.push(value.clone().map(Value::Text));
            format!("{} = {}", quote_ident(driver, &col.name), placeholder(driver, params.len(), col, None))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let cond = key_conditions(driver, key, &mut params);
    Statement {
        sql: format!("UPDATE {} SET {assignments} WHERE {cond}", qualified(driver, table)),
        params,
        expect_one: true,
    }
}

pub fn delete_row(driver: Driver, table: &TableRef, key: &[(&ColumnInfo, Option<Value>)]) -> Statement {
    let mut params = Vec::new();
    let cond = key_conditions(driver, key, &mut params);
    Statement { sql: format!("DELETE FROM {} WHERE {cond}", qualified(driver, table)), params, expect_one: true }
}

pub fn insert_row(driver: Driver, table: &TableRef, values: &[(&ColumnInfo, Option<String>)]) -> Statement {
    let target = qualified(driver, table);
    if values.is_empty() {
        let sql = match driver {
            Driver::MySql => format!("INSERT INTO {target} () VALUES ()"),
            Driver::Postgres | Driver::Sqlite => format!("INSERT INTO {target} DEFAULT VALUES"),
        };
        return Statement { sql, params: vec![], expect_one: false };
    }
    let mut params = Vec::new();
    let mut cols = Vec::new();
    let mut holders = Vec::new();
    for (col, value) in values {
        params.push(value.clone().map(Value::Text));
        cols.push(quote_ident(driver, &col.name));
        holders.push(placeholder(driver, params.len(), col, None));
    }
    Statement {
        sql: format!("INSERT INTO {target} ({}) VALUES ({})", cols.join(", "), holders.join(", ")),
        params,
        expect_one: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// Unquoted identifier or keyword, upper-cased.
    Word(String),
    /// String, quoted identifier, number or parameter.
    Literal,
    /// Any other single character (operators, `;`, `(`…).
    Punct(char),
    Comment,
}

/// Why lexing stopped before the end of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// A quote or comment is never closed. The server rejects such text or
    /// reads the rest as part of it, so at most what comes before can run.
    Unterminated(&'static str),
    /// Text whose meaning cannot be checked.
    Unsupported(&'static str),
}

/// Minimal SQL lexer: knows each dialect's quotes, escapes and comments well
/// enough to find keywords and `;` that sit outside of literals.
///
/// It must split text exactly where the server does: if it took a quote or
/// comment to end later than the server, statements hidden inside would
/// slip past the checks. Some rules depend on session settings (MySQL
/// `sql_mode`, PostgreSQL `standard_conforming_strings`), so text is lexed
/// under every variant the server could be using and each result must pass.
#[derive(Debug, Clone, Copy)]
struct Lexer {
    driver: Driver,
    /// `\` escapes the next character in '…' (MySQL by default, PostgreSQL
    /// with `standard_conforming_strings = off`).
    escape_single: bool,
    /// `\` escapes the next character in "…" (MySQL without `ANSI_QUOTES`).
    escape_double: bool,
}

impl Lexer {
    fn variants(driver: Driver) -> Vec<Lexer> {
        let lexer = |escape_single, escape_double| Lexer { driver, escape_single, escape_double };
        match driver {
            // standard_conforming_strings on (the default) and off.
            Driver::Postgres => vec![lexer(false, false), lexer(true, false)],
            // The default, ANSI_QUOTES and NO_BACKSLASH_ESCAPES.
            Driver::MySql => vec![lexer(true, true), lexer(true, false), lexer(false, false)],
            Driver::Sqlite => vec![lexer(false, false)],
        }
    }

    /// The tokens up to the end of `sql`, or up to where lexing stopped.
    fn tokenize(self, sql: &str) -> (Vec<Token>, Option<Stop>) {
        let mut tokens = Vec::new();
        let stop = self.lex(sql, &mut tokens).err();
        (tokens, stop)
    }

    fn lex(self, sql: &str, tokens: &mut Vec<Token>) -> Result<(), Stop> {
        let (pg, my, lite) =
            (self.driver == Driver::Postgres, self.driver == Driver::MySql, self.driver == Driver::Sqlite);
        let chars: Vec<char> = sql.chars().collect();
        let at = |i: usize| chars.get(i).copied();
        let mut i = 0;
        while let Some(c) = at(i) {
            let next = at(i + 1);
            if c == '\0' {
                return Err(Stop::Unsupported("a NUL character"));
            } else if is_space(c) {
                i += 1;
            } else if (c == '-'
                && next == Some('-')
                && (!my || at(i + 2).is_none_or(|n| n == ' ' || n.is_ascii_control())))
                || (c == '#' && my)
            {
                // MySQL needs a space after `--` (`1--1` is arithmetic) and
                // also starts comments with `#`. Only PostgreSQL ends them at `\r`.
                while at(i).is_some_and(|c| c != '\n' && !(pg && c == '\r')) {
                    i += 1;
                }
                tokens.push(Token::Comment);
            } else if c == '/' && next == Some('*') {
                if my && (at(i + 2) == Some('!') || (at(i + 2) == Some('M') && at(i + 3) == Some('!'))) {
                    // The server runs their content, depending on its version.
                    return Err(Stop::Unsupported("an executable comment (/*! … */)"));
                }
                i = self.comment_end(&chars, i)?;
                tokens.push(Token::Comment);
            } else if c == '\'' || c == '"' || (c == '`' && !pg) || (c == '[' && lite) {
                let escapes = match c {
                    '\'' => self.escape_single,
                    '"' => my && self.escape_double,
                    _ => false,
                };
                i = quoted_end(&chars, i, escapes)?;
                tokens.push(Token::Literal);
            } else if c == '$' && pg {
                match dollar_end(&chars, i)? {
                    Some(end) => {
                        i = end;
                        tokens.push(Token::Literal);
                    }
                    None => {
                        i += 1;
                        tokens.push(Token::Punct('$'));
                    }
                }
            } else if lite && matches!(c, '$' | '@' | ':' | '#') && next.is_some_and(is_ident_char) {
                i = sqlite_parameter_end(&chars, i);
                tokens.push(Token::Literal);
            } else if is_word_start(c) || (my && c == '$') {
                let start = i;
                while at(i).is_some_and(is_ident_char) {
                    i += 1;
                }
                let word = chars[start..i].iter().collect::<String>().to_ascii_uppercase();
                // PostgreSQL string prefixes: E'…' always takes backslash
                // escapes, B'…' and X'…' never do.
                let prefix_escapes = match word.as_str() {
                    "E" => Some(true),
                    "B" | "X" => Some(false),
                    _ => None,
                };
                if pg
                    && at(i) == Some('\'')
                    && let Some(escapes) = prefix_escapes
                {
                    i = quoted_end(&chars, i, escapes)?;
                    tokens.push(Token::Literal);
                } else {
                    tokens.push(Token::Word(word));
                }
            } else if c.is_ascii_digit() {
                while at(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                tokens.push(Token::Literal);
            } else {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        }
        Ok(())
    }

    /// Index just past the `/* … */` comment starting at `start`. Only
    /// PostgreSQL nests them.
    fn comment_end(self, chars: &[char], start: usize) -> Result<usize, Stop> {
        let nested = self.driver == Driver::Postgres;
        let mut depth = 0;
        let mut i = start;
        while i < chars.len() {
            if chars[i] == '/' && chars.get(i + 1) == Some(&'*') && (depth == 0 || nested) {
                depth += 1;
                i += 2;
            } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Ok(i);
                }
            } else {
                i += 1;
            }
        }
        Err(Stop::Unterminated("an unterminated comment"))
    }
}

/// Whitespace as the servers see it: ASCII only. Other spaces such as
/// U+00A0 are part of identifiers.
fn is_space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\x0b'
}

fn is_word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

/// Every dialect allows `$` and any non-ASCII character inside identifiers.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Index just past the quoted text starting at `start`. Doubled quotes stand
/// for the quote itself, except in SQLite's `[name]`.
fn quoted_end(chars: &[char], start: usize, escapes: bool) -> Result<usize, Stop> {
    let close = if chars[start] == '[' { ']' } else { chars[start] };
    let mut i = start + 1;
    while i < chars.len() {
        if escapes && chars[i] == '\\' {
            i += 2;
        } else if chars[i] == close {
            if close != ']' && chars.get(i + 1) == Some(&close) {
                i += 2;
            } else {
                return Ok(i + 1);
            }
        } else {
            i += 1;
        }
    }
    Err(Stop::Unterminated("an unterminated string or quoted name"))
}

/// PostgreSQL `$1` parameters and `$tag$ … $tag$` strings: returns the index
/// just past them, or `None` for a lone `$`.
fn dollar_end(chars: &[char], start: usize) -> Result<Option<usize>, Stop> {
    let mut j = start + 1;
    if chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
        }
        return Ok(Some(j));
    }
    if chars.get(j).is_some_and(|&c| is_word_start(c)) {
        while chars.get(j).is_some_and(|&c| is_ident_char(c) && c != '$') {
            j += 1;
        }
    }
    if chars.get(j) != Some(&'$') {
        return Ok(None);
    }
    let tag = &chars[start..=j];
    (j + 1..chars.len())
        .find(|&k| chars[k..].starts_with(tag))
        .map(|k| Some(k + tag.len()))
        .ok_or(Stop::Unterminated("an unterminated $-quoted string"))
}

/// SQLite parameters (`:name`, `@name`, `$name`, `#name`), including the Tcl
/// forms `$a::b` and `$name(…)`, whose parentheses may hold anything up to a
/// space or `)`.
fn sqlite_parameter_end(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    loop {
        match chars.get(i) {
            Some(&c) if is_ident_char(c) => i += 1,
            Some(':') if chars.get(i + 1) == Some(&':') => i += 2,
            Some('(') => {
                i += 1;
                while chars.get(i).is_some_and(|&c| !is_space(c) && c != ')') {
                    i += 1;
                }
                return if chars.get(i) == Some(&')') { i + 1 } else { i };
            }
            _ => return i,
        }
    }
}

const READ_STATEMENTS: &[&str] =
    &["SELECT", "WITH", "SHOW", "EXPLAIN", "DESCRIBE", "DESC", "VALUES", "TABLE", "PRAGMA", "HELP", "USE"];
/// Keywords that turn an otherwise reading statement into a write
/// (data-modifying CTEs, `SELECT ... INTO`, `EXPLAIN ANALYZE <dml>`), plus
/// PostgreSQL's `set_config()`, the function form of `SET`.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "TRUNCATE",
    "INTO",
    "DROP",
    "ALTER",
    "CREATE",
    "GRANT",
    "REVOKE",
    "COPY",
    "SET_CONFIG",
];
/// SQLite pragmas whose argument only says what to read.
const READ_PRAGMAS: &[&str] = &[
    "TABLE_INFO",
    "TABLE_XINFO",
    "TABLE_LIST",
    "INDEX_INFO",
    "INDEX_XINFO",
    "INDEX_LIST",
    "FOREIGN_KEY_LIST",
    "FOREIGN_KEY_CHECK",
    "INTEGRITY_CHECK",
    "QUICK_CHECK",
];

/// Returns `None` when every statement in `sql` only reads data, otherwise
/// what makes it unsafe: the keyword that writes, or a description of text
/// that cannot be checked. Used to refuse writes up front for read-only
/// sessions.
///
/// Only statements that start with a reading keyword pass, so transaction
/// control (`BEGIN`, `COMMIT`, `END`…) and settings (`SET`, `RESET`) are
/// refused as well: the read-only transaction that `Database::run_query`
/// wraps around the text must not be ended or reconfigured from inside it.
pub fn write_keyword(driver: Driver, sql: &str) -> Option<String> {
    let mut unterminated = None;
    let mut complete = false;
    for lexer in Lexer::variants(driver) {
        let (tokens, stop) = lexer.tokenize(sql);
        // After an unterminated quote the server stops too: what it could
        // run is in `tokens`.
        if let Some(keyword) = write_in(&tokens) {
            return Some(keyword);
        }
        match stop {
            None => complete = true,
            Some(Stop::Unterminated(what)) => unterminated = Some(what),
            Some(Stop::Unsupported(what)) => return Some(what.into()),
        }
    }
    // Broken under every reading: refuse with a clear reason instead of a
    // server syntax error.
    if complete { None } else { unterminated.map(Into::into) }
}

fn write_in(tokens: &[Token]) -> Option<String> {
    for statement in tokens.split(|t| *t == Token::Punct(';')) {
        let words: Vec<&str> = statement
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w.as_str()),
                _ => None,
            })
            .collect();
        let Some(first) = words.first() else { continue };
        if !READ_STATEMENTS.contains(first) {
            return Some(first.to_string());
        }
        if *first == "PRAGMA" {
            if pragma_writes(statement) {
                return Some("PRAGMA".into());
            }
            continue;
        }
        // Writes hidden inside reads: CTEs with DML, SELECT INTO, row locks...
        for (idx, word) in words.iter().enumerate() {
            if WRITE_KEYWORDS.contains(word) {
                // Deliberately strict: a false positive only means the user
                // sees a refusal instead of a server-side permission error.
                return Some(word.to_string());
            }
            let rest = &words[idx + 1..];
            if *word == "FOR" {
                let lock: Vec<&str> = rest
                    .iter()
                    .take(3)
                    .take_while(|w| matches!(**w, "UPDATE" | "SHARE" | "NO" | "KEY"))
                    .copied()
                    .collect();
                if lock.last().is_some_and(|w| matches!(*w, "UPDATE" | "SHARE")) {
                    return Some(format!("FOR {}", lock.join(" ")));
                }
            }
            if *word == "LOCK" && rest.starts_with(&["IN", "SHARE", "MODE"]) {
                return Some("LOCK IN SHARE MODE".into());
            }
        }
    }
    None
}

/// `PRAGMA name = value` and `PRAGMA name(value)` change settings, except for
/// pragmas such as `table_info(t)`; a plain `PRAGMA name` reads.
fn pragma_writes(statement: &[Token]) -> bool {
    if statement.contains(&Token::Punct('=')) {
        return true;
    }
    let Some(paren) = statement.iter().position(|t| *t == Token::Punct('(')) else { return false };
    let name = statement[..paren].iter().rev().find_map(|t| match t {
        Token::Word(w) => Some(w.as_str()),
        _ => None,
    });
    !name.is_some_and(|n| READ_PRAGMAS.contains(&n))
}

/// Checks a user-supplied `WHERE` condition: it must be a single expression
/// that only reads, so it cannot be used to smuggle in another statement or
/// to escape the parentheses it is placed in.
pub fn validate_filter(driver: Driver, filter: &str) -> Result<(), String> {
    let mut unterminated = None;
    let mut complete = false;
    for lexer in Lexer::variants(driver) {
        let (tokens, stop) = lexer.tokenize(filter);
        let mut depth = 0usize;
        for token in &tokens {
            match token {
                Token::Punct(';') => return Err("a filter cannot contain ';'".into()),
                Token::Comment => return Err("a filter cannot contain comments".into()),
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth = depth.checked_sub(1).ok_or("a filter cannot close a parenthesis it did not open")?
                }
                _ => {}
            }
        }
        match stop {
            None if depth > 0 => return Err("a filter must close every parenthesis it opens".into()),
            None => complete = true,
            Some(Stop::Unterminated(what)) => unterminated = Some(what),
            Some(Stop::Unsupported(what)) => return Err(format!("a filter cannot contain {what}")),
        }
    }
    if !complete && let Some(what) = unterminated {
        return Err(format!("a filter cannot contain {what}"));
    }
    match write_keyword(driver, &format!("SELECT * FROM t WHERE ({filter})")) {
        Some(keyword) => Err(format!("a filter cannot contain {keyword}")),
        None => Ok(()),
    }
}

#[cfg(test)]
pub fn is_read_only(driver: Driver, sql: &str) -> bool {
    write_keyword(driver, sql).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), ..Default::default() }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident(Driver::Postgres, "we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident(Driver::MySql, "we`ird"), "`we``ird`");
        assert_eq!(qualified(Driver::Sqlite, &TableRef::new("main", "t")), "\"t\"");
        assert_eq!(qualified(Driver::Postgres, &TableRef::new("public", "t")), "\"public\".\"t\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn page_query() {
        let req = PageRequest {
            filter: Some("id > 3".into()),
            order_by: Some(("name".into(), false)),
            default_order: vec!["id".into()],
            limit: 50,
            offset: 100,
        };
        assert_eq!(
            select_page(Driver::MySql, &TableRef::new("shop", "users"), &req),
            "SELECT * FROM `shop`.`users` WHERE (id > 3) ORDER BY `name` DESC LIMIT 50 OFFSET 100"
        );
        let req = PageRequest { filter: Some("  ".into()), limit: 10, ..Default::default() };
        assert_eq!(
            select_page(Driver::Sqlite, &TableRef::new("main", "t"), &req),
            "SELECT * FROM \"t\" LIMIT 10 OFFSET 0"
        );
    }

    #[test]
    fn update_postgres_casts_parameters() {
        let id = ColumnInfo { base_type: "integer".into(), ..col("id", "integer") };
        // Cast to the base type: `CAST($1 AS character varying(40))` would
        // silently truncate a longer value instead of failing.
        let name = ColumnInfo { base_type: "character varying".into(), ..col("name", "character varying(40)") };
        let st = update_row(
            Driver::Postgres,
            &TableRef::new("public", "users"),
            &[(&id, Some("7".into()))],
            &[(&name, Some("Ada".into()))],
        );
        assert_eq!(
            st.sql,
            "UPDATE \"public\".\"users\" SET \"name\" = CAST($1 AS character varying) WHERE \"id\" = CAST($2 AS integer)"
        );
        assert_eq!(st.params, vec![Some("Ada".into()), Some("7".into())]);
        assert!(st.expect_one);
        let code = ColumnInfo { base_type: "bpchar".into(), ..col("code", "character(3)") };
        let st = insert_row(Driver::Postgres, &TableRef::new("public", "t"), &[(&code, Some("abcd".into()))]);
        assert_eq!(st.sql, "INSERT INTO \"public\".\"t\" (\"code\") VALUES (CAST($1 AS bpchar))");
    }

    #[test]
    fn delete_and_insert_mysql() {
        let id = col("id", "int");
        let t = TableRef::new("shop", "users");
        let st = delete_row(Driver::MySql, &t, &[(&id, Some("1".into()))]);
        assert_eq!(st.sql, "DELETE FROM `shop`.`users` WHERE `id` = ?");
        let name = col("name", "varchar(10)");
        let st = insert_row(Driver::MySql, &t, &[(&name, None)]);
        assert_eq!(st.sql, "INSERT INTO `shop`.`users` (`name`) VALUES (?)");
        assert_eq!(st.params, vec![None]);
        assert_eq!(insert_row(Driver::MySql, &t, &[]).sql, "INSERT INTO `shop`.`users` () VALUES ()");
        assert_eq!(
            insert_row(Driver::Sqlite, &TableRef::new("main", "t"), &[]).sql,
            "INSERT INTO \"t\" DEFAULT VALUES"
        );
    }

    #[test]
    fn null_keys_use_is_null() {
        let a = col("a", "text");
        let st = delete_row(Driver::Sqlite, &TableRef::new("main", "t"), &[(&a, None)]);
        assert_eq!(st.sql, "DELETE FROM \"t\" WHERE \"a\" IS NULL");
        assert!(st.params.is_empty());
    }

    #[test]
    fn classify_reads() {
        for driver in Driver::ALL {
            for sql in [
                "select * from t",
                "  -- comment\n SELECT 1;",
                "/* hi */ with x as (select 1) select * from x",
                "SHOW TABLES",
                "explain select 1",
                "select 'insert into t' as s",
                "select \"update\" from t",
                "PRAGMA table_info(t)",
                "PRAGMA main.index_list(\"t\")",
                "PRAGMA query_only",
                "select replace(name, 'a', 'b') from t where x = 1",
                "select 1; select 2;",
                "select 'it''s' as s",
                "",
            ] {
                assert!(is_read_only(driver, sql), "{driver:?}: {sql}");
            }
        }
        for sql in ["select $$ delete $$", "select $q$ ; drop table t $q$", "select $1 where a$b = 1", "select 1 # 2"] {
            assert!(is_read_only(Driver::Postgres, sql), "{sql}");
        }
        for sql in ["select `delete` from t", "select 1 # ; delete from t", "select 1 -- ; delete from t"] {
            assert!(is_read_only(Driver::MySql, sql), "{sql}");
        }
        // A backslash before a quote means different things depending on
        // session settings; accept it when every reading only reads.
        assert!(is_read_only(Driver::MySql, "select * from t where name = 'O\\'Brien'"));
        assert!(is_read_only(Driver::Postgres, r"select * from t where path = 'C:\dir\'"));
        assert!(is_read_only(Driver::Postgres, r"select E'it\'s'"));
        assert!(is_read_only(Driver::Sqlite, "select [delete] from t"));
    }

    #[test]
    fn classify_writes() {
        for driver in Driver::ALL {
            for (sql, kw) in [
                ("update t set a = 1", "UPDATE"),
                ("select 1; delete from t", "DELETE"),
                ("with x as (delete from t returning *) select * from x", "DELETE"),
                ("select * into backup from t", "INTO"),
                ("select * from t for update", "FOR UPDATE"),
                ("select * from t for key share", "FOR KEY SHARE"),
                ("select * from t lock in share mode", "LOCK IN SHARE MODE"),
                ("explain analyze insert into t values (1)", "INSERT"),
                ("drop table t", "DROP"),
                ("PRAGMA journal_mode = WAL", "PRAGMA"),
                ("PRAGMA query_only(0)", "PRAGMA"),
                ("PRAGMA main.query_only = OFF", "PRAGMA"),
                ("select 'a'; insert into t values ('b')", "INSERT"),
                ("select 'unterminated", "an unterminated string or quoted name"),
                ("select 1 /* unterminated", "an unterminated comment"),
                ("select 1\0; delete from t", "a NUL character"),
            ] {
                assert_eq!(write_keyword(driver, sql).as_deref(), Some(kw), "{driver:?}: {sql}");
            }
        }
    }

    #[test]
    fn transaction_control_and_settings_are_refused() {
        // These would end or loosen the read-only transaction around the
        // user's SQL, so every driver refuses them for read-only sessions.
        for driver in Driver::ALL {
            for (sql, kw) in [
                ("BEGIN; COMMIT", "BEGIN"),
                ("start transaction read write", "START"),
                ("select 1; commit; insert into t values (1)", "COMMIT"),
                ("select 1; end", "END"),
                ("rollback", "ROLLBACK"),
                ("abort", "ABORT"),
                ("savepoint s", "SAVEPOINT"),
                ("release s", "RELEASE"),
                ("set default_transaction_read_only = off", "SET"),
                ("SET SESSION TRANSACTION READ WRITE", "SET"),
                ("reset all", "RESET"),
                ("select set_config('default_transaction_read_only', 'off', false)", "SET_CONFIG"),
                ("PRAGMA query_only = OFF", "PRAGMA"),
                ("PRAGMA query_only(false)", "PRAGMA"),
                ("do $$ begin end $$", "DO"),
                ("call p()", "CALL"),
            ] {
                assert_eq!(write_keyword(driver, sql).as_deref(), Some(kw), "{driver:?}: {sql}");
            }
        }
    }

    /// Audit H1: a backslash before a quote ends a PostgreSQL / SQLite string
    /// literal, so text the old lexer took for string content ran as SQL.
    #[test]
    fn backslash_quote_cannot_hide_statements() {
        let filter = r"name='\'); DELETE FROM people; --";
        let editor = r"SELECT 1 WHERE 'a'='\'; DELETE FROM t; --'";
        let escape = r"SELECT 'x\'; COMMIT; BEGIN READ WRITE; CREATE TABLE pwned (id int); COMMIT; --'";
        let set_off = r"message='\'); SET default_transaction_read_only = off; COMMIT; INSERT INTO log SELECT 1; --";
        for driver in Driver::ALL {
            assert!(validate_filter(driver, filter).is_err(), "{driver:?}");
            assert!(validate_filter(driver, set_off).is_err(), "{driver:?}");
            assert_eq!(write_keyword(driver, editor).as_deref(), Some("DELETE"), "{driver:?}");
            assert!(write_keyword(driver, escape).is_some(), "{driver:?}");
        }
        assert_eq!(write_keyword(Driver::Postgres, escape).as_deref(), Some("COMMIT"));
        // MySQL with NO_BACKSLASH_ESCAPES / ANSI_QUOTES reads `\` like PostgreSQL.
        assert_eq!(write_keyword(Driver::MySql, r#"SELECT "\"; DELETE FROM t; -- ""#).as_deref(), Some("DELETE"));
        // PostgreSQL with standard_conforming_strings = off reads it like MySQL.
        assert_eq!(write_keyword(Driver::Postgres, r"SELECT '\'' ; DELETE FROM t; --'").as_deref(), Some("DELETE"));
        // E'' strings always escape; B'' strings never do.
        assert_eq!(write_keyword(Driver::Postgres, r"SELECT E'\''; DELETE FROM t").as_deref(), Some("DELETE"));
        assert_eq!(write_keyword(Driver::Postgres, r"SELECT B'\'; DELETE FROM t; --'").as_deref(), Some("DELETE"));
    }

    /// Other places where a lexer that ignores the dialect ends a literal or
    /// comment later than the server does.
    #[test]
    fn dialect_specific_quotes_and_comments() {
        let refused = |driver, sql| write_keyword(driver, sql).is_some();
        // `#` is an operator in PostgreSQL and a parameter prefix in SQLite.
        assert!(refused(Driver::Postgres, "SELECT 1 # 2; DELETE FROM t"));
        assert!(refused(Driver::Sqlite, "SELECT 1; #a; DELETE FROM t"));
        // MySQL needs a space after `--`.
        assert!(refused(Driver::MySql, "SELECT 1--1; DELETE FROM t"));
        // Backticks quote only in MySQL and SQLite; `[…]` only in SQLite.
        assert!(refused(Driver::Postgres, "SELECT 1 ` ; DELETE FROM t; `"));
        assert!(refused(Driver::Sqlite, "SELECT 1 AS [a'b]; DELETE FROM t; -- ']"));
        // Dollar quotes only in PostgreSQL, and `$` continues identifiers.
        assert!(refused(Driver::MySql, "SELECT $a$; DELETE FROM t; $a$"));
        assert!(refused(Driver::Postgres, "SELECT 1 AS a$b$; DELETE FROM t; --$b$"));
        assert!(refused(Driver::Postgres, "SELECT 1 AS x\u{a0}$t$; DELETE FROM t; --$t$"));
        // PostgreSQL nests block comments and ends line comments at `\r`.
        assert!(refused(Driver::Postgres, "SELECT 1 /* /* */ ' */ ; DELETE FROM t; -- '"));
        assert!(refused(Driver::Postgres, "SELECT 1; --x\r; DELETE FROM t"));
        assert!(!refused(Driver::Sqlite, "SELECT 1; --x\r; DELETE FROM t"));
        // SQLite's Tcl-style parameters may contain quotes.
        assert!(refused(Driver::Sqlite, "SELECT $a(') ; DELETE FROM t ; --'"));
        // MySQL runs the body of executable comments.
        assert_eq!(
            write_keyword(Driver::MySql, "SELECT /*!50000 1 */").as_deref(),
            Some("an executable comment (/*! … */)")
        );
    }

    #[test]
    fn filters_must_be_plain_conditions() {
        for driver in Driver::ALL {
            assert!(validate_filter(driver, "name like 'a;b' and id > 3").is_ok());
            assert!(validate_filter(driver, "(a = 1 or b = 2) and c in (1, 2)").is_ok());
            assert!(validate_filter(driver, "").is_ok());
            assert!(validate_filter(driver, "1=1; delete from t").is_err());
            assert!(validate_filter(driver, "id in (select id from t for update)").is_err());
            // Leaving the surrounding `WHERE (…)` or commenting it out.
            assert!(validate_filter(driver, "1=1) --").is_err());
            assert!(validate_filter(driver, "1=1) or (1=1").is_err());
            assert!(validate_filter(driver, "(1=1").is_err());
            assert!(validate_filter(driver, "id > 1 /* note */").is_err());
            assert!(validate_filter(driver, "name = 'open").is_err());
        }
        assert!(validate_filter(Driver::MySql, "name = 'O\\'Brien'").is_ok());
        assert!(validate_filter(Driver::MySql, "id > 1 # note").is_err());
        assert!(validate_filter(Driver::Postgres, "a # b = 0").is_ok());
    }

    #[test]
    fn preview_inlines_params() {
        let st =
            Statement { sql: "UPDATE t SET a = ?".into(), params: vec![Some("x'y".into()), None], expect_one: true };
        assert_eq!(st.preview(), "UPDATE t SET a = ?;  -- ['x''y', NULL]");
    }

    /// Audit L8: a key is bound as what it was read as, so that it matches
    /// exactly the row it was read from, and text that does not give the
    /// value back exactly is never used as a key.
    #[test]
    fn keys_are_bound_as_read() {
        assert_eq!(Value::from_cell("1", CellKind::Integer), Some(Value::Integer(1)));
        assert_eq!(Value::from_cell("1", CellKind::Text), Some(Value::Text("1".into())));
        assert_eq!(Value::from_cell("0xcafe", CellKind::Bytes), Some(Value::Bytes(vec![0xca, 0xfe])));
        assert_eq!(Value::from_cell("0x", CellKind::Bytes), Some(Value::Bytes(vec![])));
        for f in [0.1, 3.0, -2.5e-320, 1e300, f64::MAX, f64::INFINITY] {
            assert_eq!(Value::from_cell(&format!("{f:?}"), CellKind::Real), Some(Value::Real(f)));
        }
        for (text, kind) in [
            ("0xcaf", CellKind::Bytes),
            ("cafe", CellKind::Bytes),
            ("0x+1", CellKind::Bytes),
            ("1.5", CellKind::Integer),
            ("1", CellKind::Inexact),
        ] {
            assert_eq!(Value::from_cell(text, kind), None, "{text} {kind:?}");
        }

        let (k, v) = (col("k", "BLOB"), col("v", "TEXT"));
        let t = TableRef::new("main", "t");
        let st =
            update_row(Driver::Sqlite, &t, &[(&k, Some(Value::Bytes(vec![0xff, 0xfe])))], &[(&v, Some("x".into()))]);
        assert_eq!(st.preview(), "UPDATE \"t\" SET \"v\" = ? WHERE \"k\" = ?;  -- ['x', x'fffe']");
        let st = delete_row(Driver::Sqlite, &t, &[(&k, Some(Value::Integer(1))), (&v, Some(Value::Real(2.5)))]);
        assert_eq!(st.preview(), "DELETE FROM \"t\" WHERE \"k\" = ? AND \"v\" = ?;  -- [1, 2.5]");
        // PostgreSQL casts text to the column type; other values have their own.
        let id = ColumnInfo { base_type: "bytea".into(), ..col("id", "bytea") };
        let st = delete_row(Driver::Postgres, &TableRef::new("public", "t"), &[(&id, Some(Value::Bytes(vec![1])))]);
        assert_eq!(st.sql, "DELETE FROM \"public\".\"t\" WHERE \"id\" = $1");
    }
}
