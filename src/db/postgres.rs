use std::collections::BTreeMap;

use anyhow::Result;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Row, ValueRef};

use super::{
    ColumnInfo, IndexInfo, Privileges, SchemaInfo, SessionInfo, TableInfo, TableKind, TableRef, TableStructure,
    Transport, hex_bytes, text_col, truthy,
};

pub(super) fn cell_text(row: &PgRow, idx: usize) -> Option<String> {
    let raw = row.try_get_raw(idx).ok()?;
    if raw.is_null() {
        return None;
    }
    // Rows come from the simple query protocol, so every value is text.
    match raw.as_str() {
        Ok(s) => Some(s.to_string()),
        Err(_) => Some(hex_bytes(raw.as_bytes().unwrap_or_default())),
    }
}

pub(super) async fn session_info(pool: &PgPool) -> Result<SessionInfo> {
    let row = sqlx::query(
        "SELECT current_user::text,
                current_setting('server_version')::text,
                current_database()::text,
                current_setting('transaction_read_only')::text,
                pg_is_in_recovery()::text,
                COALESCE((SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user), false)::text",
    )
    .fetch_one(pool)
    .await?;
    let read_only_reason = if truthy(text_col(&row, 4).as_deref()) {
        Some("server is a read replica (hot standby)".to_string())
    } else if truthy(text_col(&row, 3).as_deref()) {
        Some("default_transaction_read_only is on".to_string())
    } else {
        None
    };
    Ok(SessionInfo {
        user: text_col(&row, 0).unwrap_or_default(),
        // e.g. "16.4 (Debian 16.4-1)" -> "16.4"
        server_version: format!(
            "PostgreSQL {}",
            text_col(&row, 1).unwrap_or_default().split_whitespace().next().unwrap_or_default()
        ),
        database: text_col(&row, 2).unwrap_or_default(),
        superuser: truthy(text_col(&row, 5).as_deref()),
        read_only_reason,
        transport: transport(pool).await,
    })
}

/// TLS state of a pooled connection (they all use the same options).
/// Servers without `pg_stat_ssl` (PostgreSQL-compatible ones) give `Unknown`.
async fn transport(pool: &PgPool) -> Transport {
    let row = sqlx::query(
        "SELECT s.ssl::text, s.version::text, s.cipher::text, (inet_server_addr() IS NULL)::text
         FROM pg_catalog.pg_stat_ssl s WHERE s.pid = pg_catalog.pg_backend_pid()",
    )
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some(row)) => transport_from(
            truthy(text_col(&row, 0).as_deref()),
            text_col(&row, 1),
            text_col(&row, 2),
            truthy(text_col(&row, 3).as_deref()),
        ),
        _ => Transport::Unknown,
    }
}

/// `socket`: the server has no address for the session, which happens only
/// on a Unix socket.
fn transport_from(ssl: bool, version: Option<String>, cipher: Option<String>, socket: bool) -> Transport {
    match (ssl, socket) {
        (true, _) => Transport::Tls { version: version.unwrap_or_default(), cipher: cipher.unwrap_or_default() },
        (false, true) => Transport::Socket,
        (false, false) => Transport::Plain,
    }
}

const SYSTEM_SCHEMA_FILTER: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema')
    AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp\\_%'";

pub(super) async fn schemas(pool: &PgPool) -> Result<Vec<SchemaInfo>> {
    let mut out: BTreeMap<String, Vec<TableInfo>> = BTreeMap::new();
    let schema_rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT n.nspname::text FROM pg_catalog.pg_namespace n WHERE {SYSTEM_SCHEMA_FILTER} ORDER BY 1"
    )))
    .fetch_all(pool)
    .await?;
    for row in schema_rows {
        out.entry(text_col(&row, 0).unwrap_or_default()).or_default();
    }

    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT n.nspname::text, c.relname::text, c.relkind::text,
                has_schema_privilege(n.oid, 'USAGE')::text,
                has_table_privilege(c.oid, 'SELECT')::text,
                has_table_privilege(c.oid, 'INSERT')::text,
                has_table_privilege(c.oid, 'UPDATE')::text,
                has_table_privilege(c.oid, 'DELETE')::text
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND {SYSTEM_SCHEMA_FILTER}
         ORDER BY 1, 2"
    )))
    .fetch_all(pool)
    .await?;
    for row in rows {
        let schema = text_col(&row, 0).unwrap_or_default();
        let kind = match text_col(&row, 2).as_deref() {
            Some("v") => TableKind::View,
            Some("m") => TableKind::MaterializedView,
            Some("f") => TableKind::Foreign,
            _ => TableKind::Table,
        };
        let usage = truthy(text_col(&row, 3).as_deref());
        let mut privileges = Privileges {
            select: usage && truthy(text_col(&row, 4).as_deref()),
            insert: usage && truthy(text_col(&row, 5).as_deref()),
            update: usage && truthy(text_col(&row, 6).as_deref()),
            delete: usage && truthy(text_col(&row, 7).as_deref()),
        };
        if kind == TableKind::MaterializedView {
            // Materialized views can only be refreshed, never edited.
            privileges = privileges.read_only();
        }
        out.entry(schema.clone()).or_default().push(TableInfo {
            table: TableRef::new(schema, text_col(&row, 1).unwrap_or_default()),
            kind,
            privileges,
        });
    }
    Ok(out.into_iter().map(|(name, tables)| SchemaInfo { name, tables }).collect())
}

pub(super) async fn structure(pool: &PgPool, table: &TableRef) -> Result<TableStructure> {
    let rows = sqlx::query(
        "SELECT a.attname::text,
                format_type(a.atttypid, a.atttypmod)::text,
                (NOT a.attnotnull)::text,
                pg_get_expr(d.adbin, d.adrelid)::text,
                EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                        WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey))::text,
                CASE WHEN a.attidentity IN ('a', 'd') THEN 'identity'
                     WHEN a.attgenerated = 's' THEN 'generated'
                     ELSE NULL END,
                -- Cast target for bound values: domains resolved to their base
                -- type, no typmod (-1 keeps bpchar from turning into char(1)).
                (WITH RECURSIVE base(oid, typtype, typbasetype) AS (
                     SELECT t.oid, t.typtype, t.typbasetype FROM pg_catalog.pg_type t WHERE t.oid = a.atttypid
                     UNION ALL
                     SELECT t.oid, t.typtype, t.typbasetype
                     FROM pg_catalog.pg_type t JOIN base b ON t.oid = b.typbasetype
                     WHERE b.typtype = 'd')
                 SELECT format_type(base.oid, -1) FROM base WHERE base.typtype <> 'd')::text
         FROM pg_catalog.pg_attribute a
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum",
    )
    .bind(&table.schema)
    .bind(&table.name)
    .fetch_all(pool)
    .await?;
    let columns = rows
        .iter()
        .map(|row| ColumnInfo {
            name: text_col(row, 0).unwrap_or_default(),
            data_type: text_col(row, 1).unwrap_or_default(),
            nullable: truthy(text_col(row, 2).as_deref()),
            default: text_col(row, 3),
            primary_key: truthy(text_col(row, 4).as_deref()),
            extra: text_col(row, 5),
            base_type: text_col(row, 6).unwrap_or_default(),
        })
        .collect();

    let rows = sqlx::query(
        "SELECT i.relname::text, pg_get_indexdef(ix.indexrelid)::text, ix.indisunique::text, ix.indisprimary::text
         FROM pg_catalog.pg_index ix
         JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
         JOIN pg_catalog.pg_class c ON c.oid = ix.indrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND c.relname = $2
         ORDER BY ix.indisprimary DESC, 1",
    )
    .bind(&table.schema)
    .bind(&table.name)
    .fetch_all(pool)
    .await?;
    let indexes = rows
        .iter()
        .map(|row| IndexInfo {
            name: text_col(row, 0).unwrap_or_default(),
            definition: text_col(row, 1).unwrap_or_default(),
            unique: truthy(text_col(row, 2).as_deref()),
            primary: truthy(text_col(row, 3).as_deref()),
        })
        .collect();
    Ok(TableStructure { columns, indexes, non_transactional_engine: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_from_pg_stat_ssl() {
        let tls = transport_from(true, Some("TLSv1.3".into()), Some("TLS_AES_256_GCM_SHA384".into()), false);
        assert_eq!(tls, Transport::Tls { version: "TLSv1.3".into(), cipher: "TLS_AES_256_GCM_SHA384".into() });
        assert_eq!(transport_from(false, None, None, false), Transport::Plain);
        assert_eq!(transport_from(false, None, None, true), Transport::Socket);
    }
}
