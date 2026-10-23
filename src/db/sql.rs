//! SQL generation and inspection helpers. Everything here is pure so it can be
//! unit tested without a database.

use super::{ColumnInfo, Driver, PageRequest, TableRef};

/// A parameterised statement. Parameters are always sent as text (or NULL)
/// and converted by the database to the column type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// The statement must affect exactly one row (updates/deletes by key).
    pub expect_one: bool,
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
                Some(v) => quote_literal(v),
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

fn placeholder(driver: Driver, n: usize, column: &ColumnInfo) -> String {
    match driver {
        // Parameters are bound as text; cast them to the real column type.
        Driver::Postgres => format!("CAST(${n} AS {})", column.data_type),
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

fn key_conditions(driver: Driver, key: &[(&ColumnInfo, Option<String>)], params: &mut Vec<Option<String>>) -> String {
    key.iter()
        .map(|(col, value)| match value {
            None => format!("{} IS NULL", quote_ident(driver, &col.name)),
            Some(v) => {
                params.push(Some(v.clone()));
                format!("{} = {}", quote_ident(driver, &col.name), placeholder(driver, params.len(), col))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn update_row(
    driver: Driver,
    table: &TableRef,
    key: &[(&ColumnInfo, Option<String>)],
    set: &[(&ColumnInfo, Option<String>)],
) -> Statement {
    let mut params = Vec::new();
    let assignments = set
        .iter()
        .map(|(col, value)| {
            params.push(value.clone());
            format!("{} = {}", quote_ident(driver, &col.name), placeholder(driver, params.len(), col))
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

pub fn delete_row(driver: Driver, table: &TableRef, key: &[(&ColumnInfo, Option<String>)]) -> Statement {
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
        params.push(value.clone());
        cols.push(quote_ident(driver, &col.name));
        holders.push(placeholder(driver, params.len(), col));
    }
    Statement {
        sql: format!("INSERT INTO {target} ({}) VALUES ({})", cols.join(", "), holders.join(", ")),
        params,
        expect_one: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Semicolon,
    Equals,
    Other,
}

/// Minimal SQL lexer: understands quotes, dollar quoting and comments well
/// enough to find keywords that sit outside of literals.
fn tokenize(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') || c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' || c == '`' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && c == '\'' {
                    i += 2;
                    continue;
                }
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            tokens.push(Token::Other);
        } else if c == '$' {
            // Dollar-quoted string: $tag$ ... $tag$
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if chars.get(j) == Some(&'$') {
                let tag: String = chars[i..=j].iter().collect();
                let rest: String = chars[j + 1..].iter().collect();
                match rest.find(&tag) {
                    Some(end) => i = j + 1 + rest[..end].chars().count() + tag.chars().count(),
                    None => i = chars.len(),
                }
            } else {
                i = j;
            }
            tokens.push(Token::Other);
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect::<String>().to_ascii_uppercase()));
        } else if c == ';' {
            tokens.push(Token::Semicolon);
            i += 1;
        } else if c == '=' {
            tokens.push(Token::Equals);
            i += 1;
        } else {
            tokens.push(Token::Other);
            i += 1;
        }
    }
    tokens
}

const READ_STATEMENTS: &[&str] =
    &["SELECT", "WITH", "SHOW", "EXPLAIN", "DESCRIBE", "DESC", "VALUES", "TABLE", "PRAGMA", "HELP", "USE"];
/// Keywords that turn an otherwise reading statement into a write
/// (data-modifying CTEs, `SELECT ... INTO`, `EXPLAIN ANALYZE <dml>`).
const WRITE_KEYWORDS: &[&str] =
    &["INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "INTO", "DROP", "ALTER", "CREATE", "GRANT", "REVOKE", "COPY"];

/// Returns `None` when every statement in `sql` only reads data, otherwise
/// the keyword that makes it a write. Used to refuse writes up front for
/// read-only sessions.
pub fn write_keyword(sql: &str) -> Option<String> {
    let tokens = tokenize(sql);
    for statement in tokens.split(|t| *t == Token::Semicolon) {
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
            // `PRAGMA name = value` changes settings; plain `PRAGMA name` reads.
            if statement.contains(&Token::Equals) {
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
            if *word == "FOR" && matches!(words.get(idx + 1), Some(&"UPDATE" | &"SHARE")) {
                return Some(format!("FOR {}", words[idx + 1]));
            }
        }
    }
    None
}

/// Checks a user-supplied `WHERE` condition: it must be a single expression
/// that only reads, so it cannot be used to smuggle in another statement.
pub fn validate_filter(filter: &str) -> Result<(), String> {
    if tokenize(filter).contains(&Token::Semicolon) {
        return Err("a filter cannot contain ';'".into());
    }
    match write_keyword(&format!("SELECT * FROM t WHERE {filter}")) {
        Some(keyword) => Err(format!("a filter cannot contain {keyword}")),
        None => Ok(()),
    }
}

#[cfg(test)]
pub fn is_read_only(sql: &str) -> bool {
    write_keyword(sql).is_none()
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
        let id = col("id", "integer");
        let name = col("name", "character varying(40)");
        let st = update_row(
            Driver::Postgres,
            &TableRef::new("public", "users"),
            &[(&id, Some("7".into()))],
            &[(&name, Some("Ada".into()))],
        );
        assert_eq!(
            st.sql,
            "UPDATE \"public\".\"users\" SET \"name\" = CAST($1 AS character varying(40)) WHERE \"id\" = CAST($2 AS integer)"
        );
        assert_eq!(st.params, vec![Some("Ada".into()), Some("7".into())]);
        assert!(st.expect_one);
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
        for sql in [
            "select * from t",
            "  -- comment\n SELECT 1;",
            "/* hi */ with x as (select 1) select * from x",
            "SHOW TABLES",
            "explain select 1",
            "select 'insert into t' as s",
            "select \"update\" from t",
            "select $$ delete $$",
            "PRAGMA table_info(t)",
            "select replace(name, 'a', 'b') from t where x = 1",
            "select 1; select 2;",
            "",
        ] {
            assert!(is_read_only(sql), "{sql}");
        }
    }

    #[test]
    fn classify_writes() {
        for (sql, kw) in [
            ("update t set a = 1", "UPDATE"),
            ("select 1; delete from t", "DELETE"),
            ("with x as (delete from t returning *) select * from x", "DELETE"),
            ("select * into backup from t", "INTO"),
            ("select * from t for update", "FOR UPDATE"),
            ("explain analyze insert into t values (1)", "INSERT"),
            ("drop table t", "DROP"),
            ("PRAGMA journal_mode = WAL", "PRAGMA"),
            ("BEGIN; COMMIT", "BEGIN"),
        ] {
            assert_eq!(write_keyword(sql).as_deref(), Some(kw), "{sql}");
        }
    }

    #[test]
    fn filters_must_be_plain_conditions() {
        assert!(validate_filter("name like 'a;b' and id > 3").is_ok());
        assert!(validate_filter("1=1; delete from t").is_err());
        assert!(validate_filter("id in (select id from t for update)").is_err());
    }

    #[test]
    fn preview_inlines_params() {
        let st =
            Statement { sql: "UPDATE t SET a = ?".into(), params: vec![Some("x'y".into()), None], expect_one: true };
        assert_eq!(st.preview(), "UPDATE t SET a = ?;  -- ['x''y', NULL]");
    }
}
