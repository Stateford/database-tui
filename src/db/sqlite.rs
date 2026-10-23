use std::path::Path;

use anyhow::Result;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{Row, TypeInfo, ValueRef};

use super::{
    CellKind, ColumnInfo, IndexInfo, Privileges, SchemaInfo, SessionInfo, TableInfo, TableKind, TableRef,
    TableStructure, Transport, capped_hex, capped_text, truthy,
};

pub(super) fn cell_text(row: &SqliteRow, idx: usize) -> Option<String> {
    cell(row, idx, usize::MAX).0
}

/// A value's text, and its storage class: a column without type affinity
/// can hold integer 1, real 1.5, text '1' and a blob side by side.
pub(super) fn cell(row: &SqliteRow, idx: usize, max: usize) -> (Option<String>, CellKind) {
    let Ok(raw) = row.try_get_raw(idx) else { return (None, CellKind::Inexact) };
    if raw.is_null() {
        return (None, CellKind::Text);
    }
    // The value's storage class, not the column's declared type.
    let ty = raw.type_info().name().to_string();
    let decoded = match ty.as_str() {
        "INTEGER" | "BOOLEAN" => {
            row.try_get_unchecked::<i64, _>(idx).ok().map(|v| (Some(v.to_string()), CellKind::Integer))
        }
        // Debug keeps the decimal point (`3.0`, not `3`) and reads back exactly.
        "REAL" => row.try_get_unchecked::<f64, _>(idx).ok().map(|v| (Some(format!("{v:?}")), CellKind::Real)),
        "BLOB" => row.try_get_unchecked::<&[u8], _>(idx).ok().map(|b| capped_hex(b, CellKind::Bytes, max)),
        _ => match row.try_get_unchecked::<&str, _>(idx) {
            Ok(s) => Some(capped_text(s, CellKind::Text, max)),
            // Text that is not UTF-8: shown as hex, which it is not.
            Err(_) => row.try_get_unchecked::<&[u8], _>(idx).ok().map(|b| capped_hex(b, CellKind::Inexact, max)),
        },
    };
    decoded.unwrap_or((None, CellKind::Inexact))
}

/// Checks write access without modifying the file.
pub(super) fn file_is_writable(path: &Path) -> bool {
    std::fs::OpenOptions::new().write(true).open(path).is_ok()
}

pub(super) async fn session_info(pool: &SqlitePool, path: &Path) -> Result<SessionInfo> {
    let row = sqlx::query("SELECT sqlite_version()").fetch_one(pool).await?;
    let version = cell_text(&row, 0).unwrap_or_default();
    let query_only = sqlx::query("PRAGMA query_only").fetch_one(pool).await.ok().and_then(|r| cell_text(&r, 0));
    let read_only_reason = if !file_is_writable(path) {
        Some("database file is not writable by this OS user".to_string())
    } else if truthy(query_only.as_deref()) {
        Some("PRAGMA query_only is enabled".to_string())
    } else {
        None
    };
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "local".into());
    Ok(SessionInfo {
        user,
        server_version: format!("SQLite {version}"),
        database: path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
        // SQLite has no users: whoever can write the file can do anything.
        superuser: false,
        read_only_reason,
        transport: Transport::File,
    })
}

pub(super) async fn schemas(pool: &SqlitePool) -> Result<Vec<SchemaInfo>> {
    let dbs =
        sqlx::query("SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq").fetch_all(pool).await?;
    let mut out = Vec::new();
    for db in dbs {
        let schema = cell_text(&db, 0).unwrap_or_else(|| "main".into());
        let sql = format!(
            "SELECT name, type FROM {}.sqlite_master
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY name",
            super::sql::quote_ident(super::Driver::Sqlite, &schema)
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(pool).await?;
        let tables = rows
            .iter()
            .map(|row| {
                let kind =
                    if cell_text(row, 1).as_deref() == Some("view") { TableKind::View } else { TableKind::Table };
                TableInfo {
                    table: TableRef::new(schema.clone(), cell_text(row, 0).unwrap_or_default()),
                    kind,
                    privileges: if kind == TableKind::View { Privileges::READ } else { Privileges::ALL },
                }
            })
            .collect();
        out.push(SchemaInfo { name: schema, tables });
    }
    Ok(out)
}

pub(super) async fn structure(pool: &SqlitePool, table: &TableRef) -> Result<TableStructure> {
    let rows = sqlx::query(
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
    )
    .bind(&table.name)
    .bind(&table.schema)
    .fetch_all(pool)
    .await?;
    let columns = rows
        .iter()
        .filter(|row| cell_text(row, 5).as_deref() != Some("1"))
        .map(|row| ColumnInfo {
            name: cell_text(row, 0).unwrap_or_default(),
            data_type: cell_text(row, 1).filter(|t| !t.is_empty()).unwrap_or_else(|| "ANY".into()),
            nullable: !truthy(cell_text(row, 2).as_deref()),
            default: cell_text(row, 3),
            primary_key: cell_text(row, 4).is_some_and(|pk| pk != "0"),
            extra: match cell_text(row, 5).as_deref() {
                Some("2" | "3") => Some("generated".into()),
                _ => None,
            },
            base_type: String::new(),
        })
        .collect();

    let rows = sqlx::query(
        "SELECT il.name, il.\"unique\", il.origin, m.sql
         FROM pragma_index_list(?1, ?2) il
         LEFT JOIN sqlite_master m ON m.type = 'index' AND m.name = il.name
         ORDER BY il.origin = 'pk' DESC, il.name",
    )
    .bind(&table.name)
    .bind(&table.schema)
    .fetch_all(pool)
    .await?;
    let mut indexes = Vec::new();
    for row in rows {
        let name = cell_text(&row, 0).unwrap_or_default();
        let definition = match cell_text(&row, 3) {
            Some(sql) => sql,
            None => {
                let cols = sqlx::query("SELECT group_concat(name, ', ') FROM pragma_index_info(?1, ?2)")
                    .bind(&name)
                    .bind(&table.schema)
                    .fetch_one(pool)
                    .await
                    .ok()
                    .and_then(|r| cell_text(&r, 0))
                    .unwrap_or_default();
                format!("({cols}) — created automatically")
            }
        };
        indexes.push(IndexInfo {
            unique: truthy(cell_text(&row, 1).as_deref()),
            primary: cell_text(&row, 2).as_deref() == Some("pk"),
            definition,
            name,
        });
    }
    Ok(TableStructure { columns, indexes, non_transactional_engine: None })
}
