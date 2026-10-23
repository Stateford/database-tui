//! End-to-end tests of the database layer against a temporary SQLite file.

use super::*;
use crate::config::ConnectionProfile;

async fn setup(read_only: bool) -> (tempfile::TempDir, ConnectionProfile, Database) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    {
        let mut p = ConnectionProfile::new(Driver::Sqlite);
        p.path = path.to_string_lossy().into_owned();
        std::fs::File::create(&path).unwrap();
        let db = Database::connect(&p, None).await.unwrap();
        db.run_query(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, photo BLOB);
             CREATE TABLE notes (body TEXT);
             CREATE VIEW top AS SELECT * FROM people WHERE score > 5;
             INSERT INTO people VALUES (2, 'Bob', 7.0, x'CAFE'), (1, 'Ann', 3.5, NULL), (3, 'Cy', NULL, NULL);",
            false,
        )
        .await
        .unwrap();
        db.close().await;
    }
    let mut p = ConnectionProfile::new(Driver::Sqlite);
    p.path = path.to_string_lossy().into_owned();
    p.read_only = read_only;
    let db = Database::connect(&p, None).await.unwrap();
    (dir, p, db)
}

#[tokio::test]
async fn introspection_and_privileges() {
    let (_dir, profile, db) = setup(false).await;
    let session = db.session_info(&profile).await.unwrap();
    assert!(session.read_only_reason.is_none());
    assert!(session.server_version.starts_with("SQLite"));

    let schemas = db.schemas().await.unwrap();
    assert_eq!(schemas.len(), 1);
    let tables: Vec<(&str, TableKind, Privileges)> =
        schemas[0].tables.iter().map(|t| (t.table.name.as_str(), t.kind, t.privileges)).collect();
    assert_eq!(
        tables,
        vec![
            ("notes", TableKind::Table, Privileges::ALL),
            ("people", TableKind::Table, Privileges::ALL),
            ("top", TableKind::View, Privileges::READ),
        ]
    );

    let s = db.structure(&TableRef::new("main", "people")).await.unwrap();
    let cols: Vec<(&str, bool, bool)> =
        s.columns.iter().map(|c| (c.name.as_str(), c.primary_key, c.nullable)).collect();
    assert_eq!(cols, vec![("id", true, true), ("name", false, false), ("score", false, true), ("photo", false, true)]);
}

#[tokio::test]
async fn paging_values_and_counts() {
    let (_dir, _profile, db) = setup(false).await;
    let table = TableRef::new("main", "people");
    let req = PageRequest { default_order: vec!["id".into()], limit: 2, ..Default::default() };
    let rs = db.fetch_page(&table, &req, false).await.unwrap();
    assert_eq!(rs.columns, vec!["id", "name", "score", "photo"]);
    assert_eq!(rs.rows[0], vec![Some("1".into()), Some("Ann".into()), Some("3.5".into()), None]);
    assert_eq!(rs.rows[1], vec![Some("2".into()), Some("Bob".into()), Some("7.0".into()), Some("0xcafe".into())]);
    assert_eq!(rs.rows.len(), 2);

    let req = PageRequest { filter: Some("score IS NULL".into()), limit: 10, ..Default::default() };
    for read_only in [false, true] {
        assert_eq!(db.fetch_page(&table, &req, read_only).await.unwrap().rows.len(), 1);
        assert_eq!(db.count_rows(&table, None, read_only).await.unwrap(), 3);
        assert_eq!(db.count_rows(&table, Some("score > 1"), read_only).await.unwrap(), 2);
    }
}

/// Audit H1: filters are pasted into the page and count queries, so in a
/// read-only session they run under the same guard as the SQL editor, and
/// the validator no longer lets a statement through.
#[tokio::test]
async fn read_only_page_loads_cannot_write() {
    let (_dir, _profile, db) = setup(false).await;
    let table = TableRef::new("main", "people");
    let payload = r"name='\'); DELETE FROM people; --";
    assert!(sql::validate_filter(Driver::Sqlite, payload).is_err());

    // Even a filter that got past validation cannot write.
    let injected = "1=1); DELETE FROM people; --";
    let req = PageRequest { filter: Some(injected.into()), limit: 10, ..Default::default() };
    let err = db.fetch_page(&table, &req, true).await.unwrap_err().to_string();
    assert!(err.contains("readonly"), "{err}");
    assert!(db.count_rows(&table, Some(injected), true).await.is_err());
    assert_eq!(db.count_rows(&table, None, false).await.unwrap(), 3);

    // The guard is lifted afterwards: the pooled connections still write.
    for _ in 0..3 {
        db.run_query("UPDATE people SET score = 1 WHERE id = 1", false).await.unwrap();
    }
}

#[tokio::test]
async fn apply_is_atomic() {
    let (_dir, _profile, db) = setup(false).await;
    let s = db.structure(&TableRef::new("main", "people")).await.unwrap();
    let table = TableRef::new("main", "people");
    let (id, name) = (&s.columns[0], &s.columns[1]);

    let ok = vec![
        sql::update_row(Driver::Sqlite, &table, &[(id, Some("1".into()))], &[(name, Some("Anna".into()))]),
        sql::insert_row(Driver::Sqlite, &table, &[(name, Some("Dee".into()))]),
    ];
    assert_eq!(db.apply(&ok).await.unwrap(), 2);

    // The second statement matches no row: everything must roll back.
    let bad = vec![
        sql::update_row(Driver::Sqlite, &table, &[(id, Some("2".into()))], &[(name, Some("Robert".into()))]),
        sql::delete_row(Driver::Sqlite, &table, &[(id, Some("999".into()))]),
    ];
    let err = db.apply(&bad).await.unwrap_err().to_string();
    assert!(err.contains("exactly 1 row"), "{err}");

    let rs = db.run_query("SELECT name FROM people ORDER BY id", false).await.unwrap();
    let names: Vec<_> = rs.rows.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(names, vec!["Anna", "Bob", "Cy", "Dee"]);
}

#[tokio::test]
async fn read_only_sessions_are_enforced_by_the_database() {
    let (_dir, profile, db) = setup(true).await;
    let session = db.session_info(&profile).await.unwrap();
    assert!(session.read_only_reason.is_some());
    // Even if the client-side checks were bypassed, writes fail.
    assert!(db.run_query("DELETE FROM people", false).await.is_err());
    assert!(db.run_query("SELECT * FROM people", true).await.is_ok());
}

#[tokio::test]
async fn read_only_query_mode_rolls_back_writes() {
    let (_dir, _profile, db) = setup(false).await;
    // `read_only = true` runs inside a query_only / read-only transaction.
    assert!(db.run_query("UPDATE people SET name = 'x'", true).await.is_err());
    let rs = db.run_query("SELECT count(*) FROM people WHERE name = 'x'", false).await.unwrap();
    assert_eq!(rs.rows[0][0].as_deref(), Some("0"));
}

#[tokio::test]
async fn query_results_and_affected_rows() {
    let (_dir, _profile, db) = setup(false).await;
    let rs = db.run_query("UPDATE people SET score = 1 WHERE id < 3", false).await.unwrap();
    assert!(rs.columns.is_empty());
    assert_eq!(rs.rows_affected, 2);
    let rs = db.run_query("SELECT 1 AS a; SELECT 'x' AS b, NULL AS c", false).await.unwrap();
    assert_eq!(rs.columns, vec!["b", "c"]);
    assert_eq!(rs.rows, vec![vec![Some("x".into()), None]]);
}
