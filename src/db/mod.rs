//! Database access layer.
//!
//! Every driver is wrapped by [`Database`], which exposes the same small set of
//! operations (introspection, paging through table data, running ad-hoc SQL and
//! applying staged edits) regardless of the backend.

mod grants;
mod mysql;
mod postgres;
pub mod sql;
mod sqlite;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod transport_tests;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, Column, Either, Executor, Row};

use crate::config::{ConnectionProfile, SslMode};

pub use sql::Statement;

/// Maximum number of rows kept from an ad-hoc query.
pub const QUERY_ROW_LIMIT: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Driver {
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
    Sqlite,
}

impl Driver {
    pub const ALL: [Driver; 3] = [Driver::Postgres, Driver::MySql, Driver::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            Driver::Postgres => "PostgreSQL",
            Driver::MySql => "MySQL / MariaDB",
            Driver::Sqlite => "SQLite",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Driver::Postgres => "PG",
            Driver::MySql => "MY",
            Driver::Sqlite => "SQ",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Driver::Postgres => Some(5432),
            Driver::MySql => Some(3306),
            Driver::Sqlite => None,
        }
    }

    pub fn is_file_based(self) -> bool {
        matches!(self, Driver::Sqlite)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { schema: schema.into(), name: name.into() }
    }
}

impl std::fmt::Display for TableRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    Foreign,
}

impl TableKind {
    pub fn label(self) -> &'static str {
        match self {
            TableKind::Table => "table",
            TableKind::View => "view",
            TableKind::MaterializedView => "materialized view",
            TableKind::Foreign => "foreign table",
        }
    }
}

/// What the connected user may do with a single table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Privileges {
    pub select: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
}

impl Privileges {
    pub const ALL: Privileges = Privileges { select: true, insert: true, update: true, delete: true };
    pub const READ: Privileges = Privileges { select: true, insert: false, update: false, delete: false };

    pub fn can_write(&self) -> bool {
        self.insert || self.update || self.delete
    }

    /// Drop every write privilege (used when the whole session is read-only).
    pub fn read_only(self) -> Privileges {
        Privileges { insert: false, update: false, delete: false, ..self }
    }
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub table: TableRef,
    pub kind: TableKind,
    pub privileges: Privileges,
}

#[derive(Debug, Clone)]
pub struct SchemaInfo {
    pub name: String,
    pub tables: Vec<TableInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct ColumnInfo {
    pub name: String,
    /// Type as shown to the user, e.g. `character varying(40)`.
    pub data_type: String,
    /// PostgreSQL only: the type that bound values are cast to. It has no
    /// length or precision modifier and domains are resolved to their base
    /// type, because an explicit cast to `varchar(40)` silently truncates;
    /// the server's assignment rules then reject values that do not fit.
    pub base_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    /// Extra information such as `auto_increment`.
    pub extra: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub definition: String,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TableStructure {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    /// MySQL storage engine of the table when it cannot roll back
    /// (MyISAM, MEMORY…): a failed commit keeps the statements before it.
    pub non_transactional_engine: Option<String>,
}

/// Rows returned by a query; every value is rendered as text, `None` is SQL NULL.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub rows_affected: u64,
    pub truncated: bool,
    pub elapsed: Duration,
}

/// How much the session may change, summarised for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevel {
    ReadOnly,
    ReadWrite,
    Superuser,
}

impl AccessLevel {
    pub fn label(self) -> &'static str {
        match self {
            AccessLevel::ReadOnly => "READ-ONLY",
            AccessLevel::ReadWrite => "READ-WRITE",
            AccessLevel::Superuser => "SUPERUSER",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub user: String,
    pub server_version: String,
    pub database: String,
    pub superuser: bool,
    /// Set when nothing can be written in this session, with the reason why.
    pub read_only_reason: Option<String>,
    /// How the session reaches the server, as the server reports it.
    pub transport: Transport,
}

/// Transport of a session, as reported by the server (`pg_stat_ssl`, MySQL
/// `Ssl_version` / `Ssl_cipher`): with `prefer`, nothing else tells whether
/// TLS was actually negotiated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Transport {
    /// The server did not say.
    #[default]
    Unknown,
    /// SQLite: a local file.
    File,
    /// Unix-domain socket.
    Socket,
    /// TCP without encryption.
    Plain,
    /// TLS; either part may be empty when the server leaves it out.
    Tls { version: String, cipher: String },
}

/// How much a session's transport can be trusted, for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportTrust {
    /// Stays on this machine.
    Local,
    /// TLS with the server's certificate and host name verified.
    Verified,
    /// Encrypted, but the server was not authenticated.
    Unverified,
    /// Not encrypted, over a network.
    Exposed,
}

impl Transport {
    /// Summary of `self` for a session opened with `profile`: a short label
    /// and how far it can be trusted. `None` for SQLite files.
    pub fn summary(&self, profile: &ConnectionProfile) -> Option<(String, TransportTrust)> {
        let tls = |version: &str| {
            let version = if version.is_empty() { "TLS" } else { version };
            match profile.ssl_mode {
                SslMode::VerifyFull => (format!("{version} verified"), TransportTrust::Verified),
                SslMode::VerifyCa => (format!("{version} host unchecked"), TransportTrust::Unverified),
                _ => (format!("{version} unverified"), TransportTrust::Unverified),
            }
        };
        let plain = || ("NOT ENCRYPTED".to_string(), TransportTrust::Exposed);
        let (label, trust) = match self {
            Transport::File => return None,
            Transport::Socket => ("socket".to_string(), TransportTrust::Local),
            Transport::Plain => plain(),
            Transport::Tls { version, .. } => tls(version),
            // sqlx refuses plain TCP from `require` on; `disable` never tries TLS.
            Transport::Unknown => match profile.ssl_mode {
                SslMode::Disable => plain(),
                SslMode::Prefer => ("TLS unknown".to_string(), TransportTrust::Unverified),
                _ => tls(""),
            },
        };
        if profile.is_local() && trust != TransportTrust::Verified {
            let label = if trust == TransportTrust::Exposed { "no TLS".to_string() } else { label };
            return Some((label, TransportTrust::Local));
        }
        Some((label, trust))
    }
}

#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    pub filter: Option<String>,
    pub order_by: Option<(String, bool)>,
    /// Columns (the primary key) used to keep paging stable when no explicit
    /// sort is chosen.
    pub default_order: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Clone)]
pub enum Database {
    Postgres(PgPool),
    MySql(MySqlPool),
    Sqlite(SqlitePool, PathBuf),
}

impl Database {
    pub fn driver(&self) -> Driver {
        match self {
            Database::Postgres(_) => Driver::Postgres,
            Database::MySql(_) => Driver::MySql,
            Database::Sqlite(..) => Driver::Sqlite,
        }
    }

    /// Connects with the profile's settings only. `password` is always sent
    /// as given (empty when `None`). sqlx also reads PostgreSQL defaults from
    /// `PG*` environment variables, which `main` removes at start-up (see
    /// [`clear_client_environment`]).
    pub async fn connect(profile: &ConnectionProfile, password: Option<&str>) -> Result<Database> {
        let timeout = Duration::from_secs(10);
        let ssl_ca = match profile.driver {
            Driver::Sqlite => None,
            _ => {
                profile.check_tls().map_err(anyhow::Error::msg)?;
                non_empty(&profile.ssl_ca).map(ca_file).transpose()?
            }
        };
        match profile.driver {
            Driver::Postgres => {
                let mut opts = PgConnectOptions::new_without_pgpass()
                    .host(non_empty(&profile.host).unwrap_or("localhost"))
                    .port(profile.port.unwrap_or(5432))
                    .application_name("dbtui")
                    .ssl_mode(match profile.ssl_mode {
                        SslMode::Disable => PgSslMode::Disable,
                        SslMode::Prefer => PgSslMode::Prefer,
                        SslMode::Require => PgSslMode::Require,
                        SslMode::VerifyCa => PgSslMode::VerifyCa,
                        SslMode::VerifyFull => PgSslMode::VerifyFull,
                    })
                    // Even when empty, so that PGPASSWORD is never sent.
                    .password(password.unwrap_or_default());
                if let Some(ca) = &ssl_ca {
                    opts = opts.ssl_root_cert(ca);
                }
                if let Some(user) = non_empty(&profile.user) {
                    opts = opts.username(user);
                }
                if let Some(db) = non_empty(&profile.database) {
                    opts = opts.database(db);
                }
                if profile.read_only {
                    // Enforced by the server for every transaction in the session.
                    opts = opts.options([("default_transaction_read_only", "on")]);
                }
                let pool = PgPoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(timeout)
                    .connect_with(opts)
                    .await
                    .context("could not connect to PostgreSQL")?;
                Ok(Database::Postgres(pool))
            }
            Driver::MySql => {
                let mut opts = MySqlConnectOptions::new()
                    .host(non_empty(&profile.host).unwrap_or("localhost"))
                    .port(profile.port.unwrap_or(3306))
                    .ssl_mode(match profile.ssl_mode {
                        SslMode::Disable => MySqlSslMode::Disabled,
                        SslMode::Prefer => MySqlSslMode::Preferred,
                        SslMode::Require => MySqlSslMode::Required,
                        SslMode::VerifyCa => MySqlSslMode::VerifyCa,
                        SslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
                    });
                if let Some(ca) = &ssl_ca {
                    opts = opts.ssl_ca(ca);
                }
                if let Some(user) = non_empty(&profile.user) {
                    opts = opts.username(user);
                }
                if let Some(pw) = password {
                    opts = opts.password(pw);
                }
                if let Some(db) = non_empty(&profile.database) {
                    opts = opts.database(db);
                }
                let read_only = profile.read_only;
                let pool = MySqlPoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(timeout)
                    .after_connect(move |conn, _meta| {
                        Box::pin(async move {
                            if read_only {
                                conn.execute("SET SESSION TRANSACTION READ ONLY").await?;
                            }
                            Ok(())
                        })
                    })
                    .connect_with(opts)
                    .await
                    .context("could not connect to MySQL")?;
                Ok(Database::MySql(pool))
            }
            Driver::Sqlite => {
                if profile.path.trim().is_empty() {
                    bail!("no database file configured");
                }
                let path = sqlite_file(&profile.path)?;
                if !path.exists() {
                    bail!("database file {} does not exist", path.display());
                }
                let opts = SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(profile.read_only || !sqlite::file_is_writable(&path))
                    .busy_timeout(Duration::from_secs(5))
                    // The file may come from anywhere. SQL stored in its schema
                    // (views, triggers, defaults…) may then only call functions
                    // SQLite marks as harmless, and corrupt pages are detected
                    // before use.
                    .pragma("trusted_schema", "OFF")
                    .pragma("cell_size_check", "ON");
                let pool = SqlitePoolOptions::new()
                    .max_connections(2)
                    .acquire_timeout(timeout)
                    .connect_with(opts)
                    .await
                    .context("could not open SQLite database")?;
                Ok(Database::Sqlite(pool, path))
            }
        }
    }

    pub async fn close(&self) {
        match self {
            Database::Postgres(p) => p.close().await,
            Database::MySql(p) => p.close().await,
            Database::Sqlite(p, _) => p.close().await,
        }
    }

    pub async fn session_info(&self, profile: &ConnectionProfile) -> Result<SessionInfo> {
        let mut info = match self {
            Database::Postgres(p) => postgres::session_info(p).await?,
            Database::MySql(p) => mysql::session_info(p).await?,
            Database::Sqlite(p, path) => sqlite::session_info(p, path).await?,
        };
        if profile.read_only {
            info.read_only_reason = Some("connection profile is marked read-only".into());
        }
        Ok(info)
    }

    /// Schemas with their tables and the current user's privileges on each table.
    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>> {
        match self {
            Database::Postgres(p) => postgres::schemas(p).await,
            Database::MySql(p) => mysql::schemas(p).await,
            Database::Sqlite(p, _) => sqlite::schemas(p).await,
        }
    }

    pub async fn structure(&self, table: &TableRef) -> Result<TableStructure> {
        match self {
            Database::Postgres(p) => postgres::structure(p, table).await,
            Database::MySql(p) => mysql::structure(p, table).await,
            Database::Sqlite(p, _) => sqlite::structure(p, table).await,
        }
    }

    /// One page of a table. `read_only` runs it like a read-only
    /// [`run_query`](Self::run_query), as the filter is user-supplied SQL.
    pub async fn fetch_page(&self, table: &TableRef, req: &PageRequest, read_only: bool) -> Result<ResultSet> {
        let sql = sql::select_page(self.driver(), table, req);
        self.run_query_limited(&sql, req.limit.max(1), read_only).await
    }

    pub async fn count_rows(&self, table: &TableRef, filter: Option<&str>, read_only: bool) -> Result<u64> {
        let sql = sql::count_rows(self.driver(), table, filter);
        let rs = self.run_query_limited(&sql, 1, read_only).await?;
        let value = rs.rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default();
        value.trim().parse::<u64>().with_context(|| format!("unexpected row count {value:?}"))
    }

    /// Run SQL typed by the user. When `read_only` is set the statement is
    /// executed inside a read-only transaction that is always rolled back.
    pub async fn run_query(&self, query: &str, read_only: bool) -> Result<ResultSet> {
        self.run_query_limited(query, QUERY_ROW_LIMIT, read_only).await
    }

    /// With `read_only`, the database itself refuses writes: the SQL runs in
    /// a read-only transaction started just for it and always rolled back
    /// (PostgreSQL, MySQL), or with `PRAGMA query_only` (SQLite). This does
    /// not depend on session defaults that the SQL could change; it relies on
    /// `sql::write_keyword` refusing text that ends the transaction.
    async fn run_query_limited(&self, query: &str, limit: usize, read_only: bool) -> Result<ResultSet> {
        if !read_only {
            return self.run_raw(query, limit).await;
        }
        match self {
            Database::Postgres(p) => {
                run_guarded(p.acquire().await?, "START TRANSACTION READ ONLY", "ROLLBACK", query, limit).await
            }
            Database::MySql(p) => {
                run_guarded(p.acquire().await?, "START TRANSACTION READ ONLY", "ROLLBACK", query, limit).await
            }
            Database::Sqlite(p, _) => {
                run_guarded(p.acquire().await?, "PRAGMA query_only = ON", "PRAGMA query_only = OFF", query, limit).await
            }
        }
    }

    /// Apply staged changes atomically. Statements flagged `expect_one` must
    /// touch exactly one row, otherwise everything is rolled back.
    pub async fn apply(&self, statements: &[Statement]) -> Result<u64> {
        match self {
            Database::Postgres(p) => {
                let mut tx = p.begin().await?;
                let mut total = 0;
                for st in statements {
                    let mut q = sqlx::query(AssertSqlSafe(st.sql.clone()));
                    for v in &st.params {
                        q = q.bind(v.clone());
                    }
                    total += check_affected(st, q.execute(&mut *tx).await?.rows_affected())?;
                }
                tx.commit().await?;
                Ok(total)
            }
            Database::MySql(p) => {
                let mut tx = p.begin().await?;
                let mut total = 0;
                for st in statements {
                    let mut q = sqlx::query(AssertSqlSafe(st.sql.clone()));
                    for v in &st.params {
                        q = q.bind(v.clone());
                    }
                    total += check_affected(st, q.execute(&mut *tx).await?.rows_affected())?;
                }
                tx.commit().await?;
                Ok(total)
            }
            Database::Sqlite(p, _) => {
                let mut tx = p.begin().await?;
                let mut total = 0;
                for st in statements {
                    let mut q = sqlx::query(AssertSqlSafe(st.sql.clone()));
                    for v in &st.params {
                        q = q.bind(v.clone());
                    }
                    total += check_affected(st, q.execute(&mut *tx).await?.rows_affected())?;
                }
                tx.commit().await?;
                Ok(total)
            }
        }
    }

    async fn run_raw(&self, query: &str, limit: usize) -> Result<ResultSet> {
        match self {
            Database::Postgres(p) => run_raw_on(p, query, limit).await,
            Database::MySql(p) => run_raw_on(p, query, limit).await,
            Database::Sqlite(p, _) => run_raw_on(p, query, limit).await,
        }
    }
}

fn check_affected(st: &Statement, affected: u64) -> Result<u64> {
    if st.expect_one && affected != 1 {
        // Not "all changes": non-transactional MySQL engines keep earlier
        // statements; the app explains that case.
        bail!(
            "expected to change exactly 1 row but {affected} would be affected; \
             the row may have been modified elsewhere. The transaction was rolled back.\n{}",
            st.sql
        );
    }
    Ok(affected)
}

/// Runs `query` on `conn` between `enter` and `leave`. A connection that
/// could not be reset is closed instead of going back to the pool.
async fn run_guarded<DB>(
    mut conn: sqlx::pool::PoolConnection<DB>,
    enter: &'static str,
    leave: &'static str,
    query: &str,
    limit: usize,
) -> Result<ResultSet>
where
    DB: sqlx::Database,
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
    DB::Row: CellText,
    DB::QueryResult: QueryResultExt,
{
    conn.execute(enter).await?;
    let result = run_raw_on(&mut *conn, query, limit).await;
    if conn.execute(leave).await.is_err() {
        conn.close_on_drop();
    }
    result
}

/// Decodes a single value to display text. Implemented per driver because the
/// wire formats differ.
trait CellText: Row {
    fn cell_text(&self, idx: usize) -> Option<String>;
}

impl CellText for sqlx::postgres::PgRow {
    fn cell_text(&self, idx: usize) -> Option<String> {
        postgres::cell_text(self, idx)
    }
}

impl CellText for sqlx::mysql::MySqlRow {
    fn cell_text(&self, idx: usize) -> Option<String> {
        mysql::cell_text(self, idx)
    }
}

impl CellText for sqlx::sqlite::SqliteRow {
    fn cell_text(&self, idx: usize) -> Option<String> {
        sqlite::cell_text(self, idx)
    }
}

/// Run SQL through the simple/text protocol so that every column type can be
/// rendered as text without knowing it up front.
async fn run_raw_on<'e, E, DB>(executor: E, query: &str, limit: usize) -> Result<ResultSet>
where
    E: Executor<'e, Database = DB>,
    DB: sqlx::Database,
    DB::Row: CellText,
    DB::QueryResult: QueryResultExt,
{
    let started = Instant::now();
    let mut stream = sqlx::raw_sql(AssertSqlSafe(query.to_string())).fetch_many(executor);
    let mut result = ResultSet::default();
    let mut current: Vec<Vec<Option<String>>> = Vec::new();
    let mut current_cols: Vec<String> = Vec::new();
    let mut truncated = false;
    while let Some(item) = stream.try_next().await? {
        match item {
            Either::Left(done) => {
                result.rows_affected += done.affected();
                if !current_cols.is_empty() {
                    result.columns = std::mem::take(&mut current_cols);
                    result.rows = std::mem::take(&mut current);
                    result.truncated = std::mem::take(&mut truncated);
                }
            }
            Either::Right(row) => {
                if current_cols.is_empty() {
                    current_cols = row.columns().iter().map(|c| c.name().to_string()).collect();
                }
                if current.len() >= limit {
                    truncated = true;
                    continue;
                }
                current.push((0..row.len()).map(|i| row.cell_text(i)).collect());
            }
        }
    }
    if !current_cols.is_empty() {
        result.columns = current_cols;
        result.rows = current;
        result.truncated = truncated;
    }
    result.elapsed = started.elapsed();
    Ok(result)
}

trait QueryResultExt {
    fn affected(&self) -> u64;
}

impl QueryResultExt for sqlx::postgres::PgQueryResult {
    fn affected(&self) -> u64 {
        self.rows_affected()
    }
}

impl QueryResultExt for sqlx::mysql::MySqlQueryResult {
    fn affected(&self) -> u64 {
        self.rows_affected()
    }
}

impl QueryResultExt for sqlx::sqlite::SqliteQueryResult {
    fn affected(&self) -> u64 {
        self.rows_affected()
    }
}

/// Text value of a column that was selected as text (introspection queries).
fn text_col<R: Row>(row: &R, idx: usize) -> Option<String>
where
    for<'r> String: sqlx::Decode<'r, R::Database>,
    for<'r> Vec<u8>: sqlx::Decode<'r, R::Database>,
    usize: sqlx::ColumnIndex<R>,
{
    use sqlx::ValueRef;
    let raw = row.try_get_raw(idx).ok()?;
    if raw.is_null() {
        return None;
    }
    match row.try_get_unchecked::<String, _>(idx) {
        Ok(s) => Some(s),
        Err(_) => row.try_get_unchecked::<Vec<u8>, _>(idx).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
    }
}

pub fn truthy(value: Option<&str>) -> bool {
    matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1" | "on" | "yes" | "y"))
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The database file of a SQLite profile, made absolute. SQLite reads a
/// name starting with `file:` as a URI whose parameters (`nolock`,
/// `immutable`, `vfs`…) change how the file is opened; an absolute path
/// never starts with it, so the name always means a file.
fn sqlite_file(path: &str) -> Result<PathBuf> {
    let path = expand_tilde(path.trim());
    std::path::absolute(&path).with_context(|| format!("invalid database file {path:?}"))
}

/// The CA certificate file of a profile, which must exist.
fn ca_file(path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(expand_tilde(path));
    if !path.is_file() {
        bail!("CA certificate file {} not found", path.display());
    }
    Ok(path)
}

/// Removes the environment variables that sqlx reads as PostgreSQL
/// connection defaults (every name starting with `PG`): `PGPASSWORD`,
/// a client certificate and key (`PGSSLCERT`/`PGSSLKEY`), extra CA roots
/// (`PGSSLROOTCERT`), `PGOPTIONS`, `PGUSER`… Variables exported for psql
/// would otherwise be used for whatever server a profile names, and sqlx
/// offers no way to unset some of them per connection.
///
/// # Safety
///
/// No other thread may exist yet (see [`std::env::remove_var`]).
pub unsafe fn clear_client_environment() {
    let names: Vec<_> = std::env::vars_os().map(|(name, _)| name).filter(|n| is_client_variable(n)).collect();
    for name in names {
        // SAFETY: the caller guarantees the process is single-threaded.
        unsafe { std::env::remove_var(name) };
    }
}

/// Environment variable names are case-insensitive on Windows.
fn is_client_variable(name: &OsStr) -> bool {
    name.as_encoded_bytes().get(..2).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"PG"))
}

fn non_empty(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = directories::BaseDirs::new()
    {
        return home.home_dir().join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}
