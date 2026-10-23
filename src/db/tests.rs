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

/// Audit (SQLite hardening): a file may come from anywhere. SQL stored in
/// its schema may only use what SQLite marks as harmless (here a view over
/// the `dbstat` virtual table), on every connection and in both open modes.
#[tokio::test]
async fn sqlite_files_are_opened_defensively() {
    let (_dir, profile, db) = setup(false).await;
    db.run_query("CREATE VIEW pages AS SELECT name FROM dbstat", false).await.unwrap();
    db.close().await;
    for read_only in [false, true] {
        let p = ConnectionProfile { read_only, ..profile.clone() };
        let db = Database::connect(&p, None).await.unwrap();
        assert_eq!(db.session_info(&p).await.unwrap().transport, Transport::File);
        for _ in 0..3 {
            let pragma = |rs: ResultSet| rs.rows[0][0].clone();
            assert_eq!(pragma(db.run_query("PRAGMA trusted_schema", false).await.unwrap()).as_deref(), Some("0"));
            assert_eq!(pragma(db.run_query("PRAGMA cell_size_check", false).await.unwrap()).as_deref(), Some("1"));
            // Typed by the user it runs; stored in the file it does not.
            assert!(db.run_query("SELECT count(*) FROM dbstat", false).await.is_ok());
            let err = db.run_query("SELECT * FROM pages", false).await.unwrap_err().to_string();
            assert!(err.contains("unsafe use of virtual table"), "{err}");
        }
        db.close().await;
    }
}

/// SQLite reads a file name starting with `file:` as a URI whose
/// parameters change how it is opened; profile paths are made absolute.
#[test]
fn sqlite_file_names_are_never_uris() {
    for name in ["file:data.db?nolock=1", "file:///tmp/x.db?immutable=1", " data.db "] {
        let path = sqlite_file(name).unwrap();
        assert!(path.is_absolute() && path.ends_with(name.trim()), "{name}: {}", path.display());
        assert!(!path.to_string_lossy().starts_with("file:"), "{name}");
    }
}

/// Audit L8: a key is bound as the value it was read as. In a SQLite
/// column without type affinity, integer 1 and text '1' (or a blob and its
/// `0x…` text) are different rows that look the same in the grid; an edit
/// or delete must reach exactly the one it was made on.
#[tokio::test]
async fn keys_match_the_row_they_were_read_from() {
    use crate::app::grid::Grid;
    let (_dir, _profile, db) = setup(false).await;
    db.run_query(
        "CREATE TABLE t (k PRIMARY KEY, v TEXT);
         INSERT INTO t VALUES (1, 'integer'), ('1', 'text'), (x'fffe', 'blob'), ('0xfffe', 'blob as text'),
                              (2.5, 'real'), ('2.5', 'real as text');",
        false,
    )
    .await
    .unwrap();
    let table = TableRef::new("main", "t");
    let structure = db.structure(&table).await.unwrap();
    let load = async || {
        let req = PageRequest { default_order: vec!["rowid".into()], limit: 50, ..Default::default() };
        let rs = db.fetch_page(&table, &req, false).await.unwrap();
        Grid::new(rs.columns, rs.rows).with_kinds(rs.kinds)
    };
    let values = async || {
        let rs = db.run_query("SELECT v FROM t ORDER BY rowid", false).await.unwrap();
        rs.rows.into_iter().map(|r| r[0].clone().unwrap()).collect::<Vec<_>>()
    };
    let grid = load().await;
    let shown: Vec<_> = grid.rows.iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(shown, ["1", "1", "0xfffe", "0xfffe", "2.5", "2.5"]);

    for row in 0..6 {
        let mut grid = load().await;
        grid.set_cell(row, 1, Some(format!("edited {row}")));
        let statements = grid.build_statements(Driver::Sqlite, &table, &structure).unwrap();
        assert_eq!(db.apply(&statements).await.unwrap(), 1, "{}", statements[0].preview());
        let after = values().await;
        for (i, v) in after.iter().enumerate() {
            assert_eq!(v.starts_with("edited"), i <= row, "row {row}: {after:?}");
        }
    }
    // Deleting the integer row leaves the text one, and the blob its text.
    for (row, left) in [(0, "edited 1"), (1, "edited 3")] {
        let mut grid = load().await;
        grid.toggle_delete(row);
        let statements = grid.build_statements(Driver::Sqlite, &table, &structure).unwrap();
        assert_eq!(db.apply(&statements).await.unwrap(), 1);
        assert_eq!(values().await[row], left);
    }

    // Text that is not UTF-8 is shown as hex, which cannot find it again.
    db.run_query("INSERT INTO t VALUES (CAST(x'fffe' AS TEXT), 'not utf-8')", false).await.unwrap();
    let mut grid = load().await;
    let row = grid.rows.len() - 1;
    assert_eq!(grid.rows[row][0].as_deref(), Some("0xfffe"));
    grid.toggle_delete(row);
    let err = grid.build_statements(Driver::Sqlite, &table, &structure).unwrap_err();
    assert!(err.contains("cannot be matched exactly"), "{err}");
}

/// Audit L7: values are cut when they are loaded, and marked as such: one at
/// a time, and more so past a budget for the whole page. The rest of each
/// row is kept, and a cut value is never used to find a row.
#[tokio::test]
async fn large_values_are_cut_when_loaded() {
    let (_dir, _profile, db) = setup(false).await;
    // A view makes values of any size, stored nowhere.
    db.run_query(
        "CREATE VIEW fat AS SELECT id, replace(hex(zeroblob(800000)), '0', 'é') AS body, zeroblob(300000) AS data, name
         FROM people",
        false,
    )
    .await
    .unwrap();
    let fat = TableRef::new("main", "fat");
    let rs = db.fetch_page(&fat, &PageRequest { limit: 10, ..Default::default() }, false).await.unwrap();
    assert_eq!(rs.rows.len(), 3);
    let (row, kinds) = (&rs.rows[0], &rs.kinds[0]);
    let body = row[1].as_deref().unwrap();
    assert_eq!((body.len(), kinds[1]), (CELL_BYTES, CellKind::Truncated(3_200_000)));
    assert!(body.chars().all(|c| c == 'é'));
    let data = row[2].as_deref().unwrap();
    assert_eq!((data.len(), kinds[2]), (CELL_BYTES, CellKind::Truncated(600_002)));
    assert!(data.starts_with("0x0000"));
    assert_eq!((row[3].as_deref(), kinds[0], kinds[3]), (Some("Ann"), CellKind::Integer, CellKind::Text));
    assert_eq!(Value::from_cell(body, kinds[1]), None);

    // Past the budget of the result, values keep only what fills a column.
    let Database::Sqlite(pool, _) = &db else { unreachable!() };
    let limits = Limits { rows: 10, cell: 1000, result: 2000 };
    let rs = run_raw_with(pool, "SELECT body, name FROM fat ORDER BY id", limits).await.unwrap();
    let lens: Vec<(usize, Option<&str>)> =
        rs.rows.iter().map(|r| (r[0].as_ref().unwrap().len(), r[1].as_deref())).collect();
    assert_eq!(lens, vec![(1000, Some("Ann")), (1000, Some("Bob")), (CELL_BYTES_PAST_BUDGET, Some("Cy"))]);
    assert!(rs.kinds.iter().all(|k| k[0] == CellKind::Truncated(3_200_000)));
    // Small values are left as they are.
    let people = TableRef::new("main", "people");
    let rs = db.fetch_page(&people, &PageRequest { limit: 10, ..Default::default() }, false).await.unwrap();
    assert!(rs.kinds.iter().flatten().all(|k| k.truncated().is_none()));
}
