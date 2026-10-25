use std::collections::BTreeMap;

use anyhow::Result;
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::{Row, TypeInfo, ValueRef};

use super::grants::Grants;
use super::{
    CellKind, ColumnInfo, IndexInfo, Privileges, QUERY_ROW_LIMIT, SchemaInfo, SessionInfo, TableInfo, TableKind,
    TableRef, TableStructure, Transport, capped_hex, capped_text, run_raw_on, text_col, truthy,
};

const SYSTEM_SCHEMAS: &str = "('information_schema', 'mysql', 'performance_schema', 'sys')";

pub(super) fn cell(row: &MySqlRow, idx: usize, max: usize) -> (Option<String>, CellKind) {
    let Ok(raw) = row.try_get_raw(idx) else { return (None, CellKind::Inexact) };
    if raw.is_null() {
        return (None, CellKind::Text);
    }
    // A BIT value arrives as its bytes, and compares as a number: no text
    // or bytes given back to the server would surely match it alone.
    let bit = raw.type_info().name() == "BIT";
    // Text protocol: numbers, dates, etc. all arrive as strings.
    let Ok(bytes) = row.try_get_unchecked::<&[u8], _>(idx) else { return (None, CellKind::Inexact) };
    match std::str::from_utf8(bytes) {
        Ok(s) => capped_text(s, if bit { CellKind::Inexact } else { CellKind::Text }, max),
        // Only binary strings are not UTF-8 (sqlx asks for utf8mb4).
        Err(_) => capped_hex(bytes, if bit { CellKind::Inexact } else { CellKind::Bytes }, max),
    }
}

async fn scalar(pool: &MySqlPool, sql: &'static str) -> Option<String> {
    let rs = run_raw_on(pool, sql, 1).await.ok()?;
    rs.rows.into_iter().next()?.into_iter().next()?
}

async fn grants(pool: &MySqlPool) -> Grants {
    // MySQL 8 roles: privileges of active roles are only listed with USING.
    let mut lines = Vec::new();
    if let Some(roles) = scalar(pool, "SELECT CURRENT_ROLE()").await
        && !roles.is_empty()
        && roles != "NONE"
        && let Ok(rs) = run_raw_on(pool, &format!("SHOW GRANTS FOR CURRENT_USER() USING {roles}"), 1000).await
    {
        lines = rs.rows;
    }
    if lines.is_empty()
        && let Ok(rs) = run_raw_on(pool, "SHOW GRANTS", 1000).await
    {
        lines = rs.rows;
    }
    let lines: Vec<String> = lines.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    Grants::parse(lines.iter().map(String::as_str))
}

pub(super) async fn session_info(pool: &MySqlPool) -> Result<SessionInfo> {
    let rs = run_raw_on(pool, "SELECT CURRENT_USER(), VERSION(), DATABASE()", 1).await?;
    let row = rs.rows.into_iter().next().unwrap_or_default();
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let grants = grants(pool).await;
    let superuser = grants.is_superuser();

    let read_only = truthy(scalar(pool, "SELECT @@global.read_only").await.as_deref());
    let super_read_only = truthy(scalar(pool, "SELECT @@global.super_read_only").await.as_deref());
    let tx_read_only = match scalar(pool, "SELECT @@session.transaction_read_only").await {
        Some(v) => truthy(Some(&v)),
        None => truthy(scalar(pool, "SELECT @@session.tx_read_only").await.as_deref()),
    };
    let read_only_reason = if super_read_only {
        Some("server is in super_read_only mode".to_string())
    } else if read_only && !superuser {
        Some("server is in read_only mode".to_string())
    } else if tx_read_only {
        Some("session transactions are read-only".to_string())
    } else {
        None
    };
    let version = get(1);
    let flavour = if version.to_ascii_lowercase().contains("mariadb") { "MariaDB" } else { "MySQL" };
    Ok(SessionInfo {
        user: get(0),
        server_version: format!("{flavour} {}", version.split('-').next().unwrap_or(&version)),
        database: get(2),
        superuser,
        read_only_reason,
        transport: transport(pool).await,
    })
}

/// TLS state of a pooled connection (they all use the same options).
async fn transport(pool: &MySqlPool) -> Transport {
    match run_raw_on(pool, "SHOW SESSION STATUS WHERE Variable_name IN ('Ssl_version', 'Ssl_cipher')", 10).await {
        Ok(rs) => transport_from(&rs.rows),
        Err(_) => Transport::Unknown,
    }
}

/// From `Variable_name, Value` rows; both values are empty without TLS.
fn transport_from(rows: &[Vec<Option<String>>]) -> Transport {
    let get = |name: &str| {
        rows.iter()
            .find(|r| r.first().cloned().flatten().is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .and_then(|r| r.get(1).cloned().flatten())
            .unwrap_or_default()
    };
    let (version, cipher) = (get("Ssl_version"), get("Ssl_cipher"));
    match (rows.is_empty(), version.is_empty() && cipher.is_empty()) {
        (true, _) => Transport::Unknown,
        (false, true) => Transport::Plain,
        (false, false) => Transport::Tls { version, cipher },
    }
}

pub(super) async fn schemas(pool: &MySqlPool) -> Result<Vec<SchemaInfo>> {
    let grants = grants(pool).await;
    let mut out: BTreeMap<String, Vec<TableInfo>> = BTreeMap::new();
    let rs = run_raw_on(
        pool,
        &format!("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN {SYSTEM_SCHEMAS}"),
        QUERY_ROW_LIMIT,
    )
    .await?;
    for row in rs.rows {
        if let Some(Some(name)) = row.into_iter().next() {
            out.entry(name).or_default();
        }
    }
    let rs = run_raw_on(
        pool,
        &format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES
             WHERE TABLE_SCHEMA NOT IN {SYSTEM_SCHEMAS} ORDER BY TABLE_SCHEMA, TABLE_NAME"
        ),
        100_000,
    )
    .await?;
    for row in rs.rows {
        let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
        let (schema, name, ty) = (get(0), get(1), get(2));
        let kind = if ty.contains("VIEW") { TableKind::View } else { TableKind::Table };
        let mut privileges = grants.privileges_for(&schema, &name);
        if privileges == Privileges::default() {
            // Visible in information_schema means some privilege exists
            // (e.g. through a role we could not resolve): assume read access.
            privileges = Privileges::READ;
        }
        out.entry(schema.clone()).or_default().push(TableInfo { table: TableRef::new(schema, name), kind, privileges });
    }
    Ok(out.into_iter().map(|(name, tables)| SchemaInfo { name, tables }).collect())
}

pub(super) async fn structure(pool: &MySqlPool, table: &TableRef) -> Result<TableStructure> {
    let rows = sqlx::query(
        "SELECT CAST(COLUMN_NAME AS CHAR), CAST(COLUMN_TYPE AS CHAR), CAST(IS_NULLABLE AS CHAR),
                CAST(COLUMN_DEFAULT AS CHAR), CAST(COLUMN_KEY AS CHAR), CAST(EXTRA AS CHAR)
         FROM information_schema.COLUMNS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
         ORDER BY ORDINAL_POSITION",
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
            primary_key: text_col(row, 4).as_deref() == Some("PRI"),
            extra: text_col(row, 5).filter(|s| !s.is_empty()),
            base_type: String::new(),
        })
        .collect();
    let rows = sqlx::query(
        "SELECT CAST(INDEX_NAME AS CHAR), CAST(MIN(NON_UNIQUE) AS CHAR),
                CAST(GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ', ') AS CHAR)
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
         GROUP BY INDEX_NAME
         ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME",
    )
    .bind(&table.schema)
    .bind(&table.name)
    .fetch_all(pool)
    .await?;
    let indexes = rows
        .iter()
        .map(|row| {
            let name = text_col(row, 0).unwrap_or_default();
            IndexInfo {
                primary: name == "PRIMARY",
                unique: text_col(row, 1).as_deref() == Some("0"),
                definition: format!("({})", text_col(row, 2).unwrap_or_default()),
                name,
            }
        })
        .collect();
    let engine = sqlx::query(
        "SELECT CAST(t.ENGINE AS CHAR), CAST(e.TRANSACTIONS AS CHAR)
         FROM information_schema.TABLES t
         LEFT JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE
         WHERE t.TABLE_SCHEMA = ? AND t.TABLE_NAME = ?",
    )
    .bind(&table.schema)
    .bind(&table.name)
    .fetch_optional(pool)
    .await?;
    let non_transactional_engine =
        engine.and_then(|row| non_transactional(text_col(&row, 0), text_col(&row, 1).as_deref()));
    Ok(TableStructure { columns, indexes, non_transactional_engine })
}

/// Returns the engine's name when it cannot roll back a transaction.
/// `transactions` is the `TRANSACTIONS` column of `information_schema.ENGINES`.
fn non_transactional(engine: Option<String>, transactions: Option<&str>) -> Option<String> {
    // Views have no engine of their own.
    let engine = engine.filter(|e| !e.is_empty())?;
    let transactional = match transactions {
        Some(flag) => flag.eq_ignore_ascii_case("YES"),
        // Not listed: only trust engines known to be transactional.
        None => matches!(engine.to_ascii_lowercase().as_str(), "innodb" | "ndbcluster" | "ndb" | "rocksdb"),
    };
    (!transactional).then_some(engine)
}

#[cfg(test)]
mod tests {
    use super::{Transport, non_transactional, transport_from};

    #[test]
    fn transport_from_session_status() {
        let row = |n: &str, v: &str| vec![Some(n.to_string()), Some(v.to_string())];
        let tls = [row("Ssl_cipher", "TLS_AES_128_GCM_SHA256"), row("Ssl_version", "TLSv1.3")];
        assert_eq!(
            transport_from(&tls),
            Transport::Tls { version: "TLSv1.3".into(), cipher: "TLS_AES_128_GCM_SHA256".into() }
        );
        assert_eq!(transport_from(&[row("Ssl_cipher", ""), row("Ssl_version", "")]), Transport::Plain);
        assert_eq!(transport_from(&[]), Transport::Unknown);
    }

    #[test]
    fn engines_without_transactions() {
        let check = |engine: Option<&str>, tx: Option<&str>| non_transactional(engine.map(Into::into), tx);
        assert_eq!(check(Some("InnoDB"), Some("YES")), None);
        assert_eq!(check(Some("MyISAM"), Some("NO")), Some("MyISAM".into()));
        assert_eq!(check(Some("MEMORY"), Some("NO")), Some("MEMORY".into()));
        assert_eq!(check(Some("InnoDB"), None), None);
        assert_eq!(check(Some("Aria"), None), Some("Aria".into()));
        assert_eq!(check(None, None), None);
    }
}
