//! Tabular data with a cell cursor and staged (uncommitted) edits.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use unicode_width::UnicodeWidthStr;

use crate::db::{CellKind, Driver, Statement, TableRef, TableStructure, Value, sql};

const MAX_COL_WIDTH: usize = 40;
const MIN_COL_WIDTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Undo {
    /// `prev` is the previous staged value (`None` = nothing staged).
    Cell {
        row: usize,
        col: usize,
        prev: Option<Option<String>>,
    },
    Delete(usize),
    Insert,
    RemoveInserted {
        index: usize,
        values: Vec<Option<Option<String>>>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Edits {
    /// Staged values for existing rows, keyed by (row, column).
    pub updates: BTreeMap<(usize, usize), Option<String>>,
    pub deleted: BTreeSet<usize>,
    /// New rows; per column `None` means "use the column default".
    pub inserted: Vec<Vec<Option<Option<String>>>>,
    history: Vec<Undo>,
}

impl Edits {
    /// Whether both stage the same changes (whatever their undo history).
    pub fn same_changes(&self, other: &Edits) -> bool {
        self.updates == other.updates && self.deleted == other.deleted && self.inserted == other.inserted
    }
}

/// A fetched row, identified by the values of its primary-key cells
/// (column name, text, kind) rather than by its position, which changes
/// whenever the table is reloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowKey(Vec<(String, Option<String>, CellKind)>);

/// How a cell should be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellState {
    Clean,
    Edited,
    Deleted,
    Inserted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellValue<'a> {
    Value(&'a str),
    Null,
    /// Unset cell of a new row: the database default will be used.
    Default,
}

#[derive(Debug, Clone, Default)]
pub struct Grid {
    /// Changes whenever the rows are replaced or the staged changes reset,
    /// so that what was built from the grid can tell it is out of date.
    pub id: u64,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// What each fetched cell's text stands for (see [`CellKind`]); a row
    /// with no entries is plain text.
    pub kinds: Vec<Vec<CellKind>>,
    pub widths: Vec<usize>,
    pub row: usize,
    pub col: usize,
    pub row_offset: usize,
    pub col_offset: usize,
    /// Number of body rows drawn last frame, used for paging.
    pub viewport_rows: usize,
    pub edits: Edits,
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Grid {
    pub fn new(columns: Vec<String>, rows: Vec<Vec<Option<String>>>) -> Self {
        let mut grid = Grid { id: next_id(), columns, rows, viewport_rows: 20, ..Default::default() };
        grid.compute_widths();
        grid
    }

    /// Sets what the cells' text stands for (see [`ResultSet::kinds`](crate::db::ResultSet::kinds)).
    pub fn with_kinds(mut self, kinds: Vec<Vec<CellKind>>) -> Self {
        self.kinds = kinds;
        self
    }

    /// What a fetched cell's text stands for.
    pub fn kind(&self, row: usize, col: usize) -> CellKind {
        self.kinds.get(row).and_then(|r| r.get(col)).copied().unwrap_or_default()
    }

    /// The key of fetched row `row`, made of the cells of `key_columns`.
    pub fn row_key(&self, row: usize, key_columns: &[String]) -> Option<RowKey> {
        let cells = self.rows.get(row)?;
        let key = key_columns
            .iter()
            .map(|name| {
                let col = self.columns.iter().position(|c| c == name)?;
                Some((name.clone(), cells.get(col)?.clone(), self.kind(row, col)))
            })
            .collect::<Option<Vec<_>>>()?;
        (!key.is_empty()).then_some(RowKey(key))
    }

    /// Where the fetched row with `key` is now, if it is there (and only
    /// once).
    pub fn find_row(&self, key: &RowKey) -> Option<usize> {
        let cols =
            key.0.iter().map(|(name, ..)| self.columns.iter().position(|c| c == name)).collect::<Option<Vec<_>>>()?;
        let mut found = (0..self.rows.len()).filter(|&row| {
            key.0
                .iter()
                .zip(&cols)
                .all(|((_, text, kind), &col)| self.rows[row].get(col) == Some(text) && self.kind(row, col) == *kind)
        });
        let row = found.next()?;
        found.next().is_none().then_some(row)
    }

    /// Drops the fetched rows (after a failed load), keeping the columns.
    pub fn clear_rows(&mut self) {
        self.rows.clear();
        self.kinds.clear();
        self.clear_edits();
    }

    fn compute_widths(&mut self) {
        self.widths = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let content = self
                    .rows
                    .iter()
                    .take(500)
                    .map(|r| r.get(i).and_then(|v| v.as_deref()).map(display_width).unwrap_or(4))
                    .max()
                    .unwrap_or(0);
                content.max(name.width()).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)
            })
            .collect();
    }

    pub fn total_rows(&self) -> usize {
        self.rows.len() + self.edits.inserted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total_rows() == 0
    }

    pub fn is_inserted(&self, row: usize) -> bool {
        row >= self.rows.len()
    }

    pub fn cell(&self, row: usize, col: usize) -> (CellValue<'_>, CellState) {
        if let Some(new_row) = row.checked_sub(self.rows.len()).and_then(|i| self.edits.inserted.get(i)) {
            let value = match new_row.get(col) {
                Some(Some(Some(v))) => CellValue::Value(v),
                Some(Some(None)) => CellValue::Null,
                _ => CellValue::Default,
            };
            return (value, CellState::Inserted);
        }
        if self.edits.deleted.contains(&row) {
            return (self.original(row, col), CellState::Deleted);
        }
        if let Some(v) = self.edits.updates.get(&(row, col)) {
            let value = match v {
                Some(v) => CellValue::Value(v),
                None => CellValue::Null,
            };
            return (value, CellState::Edited);
        }
        (self.original(row, col), CellState::Clean)
    }

    fn original(&self, row: usize, col: usize) -> CellValue<'_> {
        match self.rows.get(row).and_then(|r| r.get(col)) {
            Some(Some(v)) => CellValue::Value(v),
            Some(None) => CellValue::Null,
            None => CellValue::Default,
        }
    }

    /// Current text of a cell (`None` for NULL/default).
    pub fn cell_text(&self, row: usize, col: usize) -> Option<String> {
        match self.cell(row, col).0 {
            CellValue::Value(v) => Some(v.to_string()),
            _ => None,
        }
    }

    pub fn current_text(&self) -> Option<String> {
        self.cell_text(self.row, self.col)
    }

    // --- cursor -------------------------------------------------------

    pub fn move_row(&mut self, delta: isize) {
        let max = self.total_rows().saturating_sub(1) as isize;
        self.row = (self.row as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn move_col(&mut self, delta: isize) {
        let max = self.columns.len().saturating_sub(1) as isize;
        self.col = (self.col as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn top(&mut self) {
        self.row = 0;
    }

    pub fn bottom(&mut self) {
        self.row = self.total_rows().saturating_sub(1);
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.columns.len().saturating_sub(1);
    }

    pub fn page(&mut self, pages: f32) {
        let step = ((self.viewport_rows.max(2) as f32) * pages) as isize;
        self.move_row(step);
    }

    // --- staged edits -------------------------------------------------

    pub fn pending_count(&self) -> usize {
        let updated_rows: BTreeSet<usize> =
            self.edits.updates.keys().map(|(r, _)| *r).filter(|r| !self.edits.deleted.contains(r)).collect();
        updated_rows.len() + self.edits.deleted.len() + self.edits.inserted.len()
    }

    pub fn has_pending(&self) -> bool {
        self.pending_count() > 0
    }

    pub fn clear_edits(&mut self) {
        self.edits = Edits::default();
        self.id = next_id();
        self.row = self.row.min(self.total_rows().saturating_sub(1));
    }

    /// Removes the staged changes that `committed` (a copy taken when they
    /// were sent to the database) holds, and only those.
    pub fn remove_committed(&mut self, committed: &Edits) {
        for (cell, value) in &committed.updates {
            if self.edits.updates.get(cell) == Some(value) {
                self.edits.updates.remove(cell);
            }
        }
        for row in &committed.deleted {
            self.edits.deleted.remove(row);
        }
        for new_row in &committed.inserted {
            if let Some(i) = self.edits.inserted.iter().position(|r| r == new_row) {
                self.edits.inserted.remove(i);
            }
        }
        // Undoing a committed change would stage it again.
        self.edits.history.clear();
        self.id = next_id();
        self.row = self.row.min(self.total_rows().saturating_sub(1));
    }

    pub fn set_cell(&mut self, row: usize, col: usize, value: Option<String>) {
        if let Some(i) = row.checked_sub(self.rows.len()) {
            if let Some(cell) = self.edits.inserted.get_mut(i).and_then(|r| r.get_mut(col)) {
                let prev = cell.replace(value);
                self.edits.history.push(Undo::Cell { row, col, prev });
            }
            return;
        }
        let original = self.rows.get(row).and_then(|r| r.get(col)).cloned().flatten();
        let prev = if original == value {
            self.edits.updates.remove(&(row, col))
        } else {
            self.edits.updates.insert((row, col), value)
        };
        self.edits.history.push(Undo::Cell { row, col, prev });
    }

    /// Resets a cell of a new row to the column default.
    pub fn set_default(&mut self, row: usize, col: usize) {
        if let Some(cell) =
            row.checked_sub(self.rows.len()).and_then(|i| self.edits.inserted.get_mut(i)).and_then(|r| r.get_mut(col))
        {
            let prev = cell.take();
            self.edits.history.push(Undo::Cell { row, col, prev });
        }
    }

    /// Marks an existing row for deletion (or un-marks it); new rows are dropped.
    pub fn toggle_delete(&mut self, row: usize) {
        if let Some(i) = row.checked_sub(self.rows.len()) {
            if i < self.edits.inserted.len() {
                let values = self.edits.inserted.remove(i);
                self.edits.history.push(Undo::RemoveInserted { index: i, values });
                self.row = self.row.min(self.total_rows().saturating_sub(1));
            }
            return;
        }
        if !self.edits.deleted.remove(&row) {
            self.edits.deleted.insert(row);
        }
        self.edits.history.push(Undo::Delete(row));
    }

    pub fn insert_row(&mut self) {
        self.edits.inserted.push(vec![None; self.columns.len()]);
        self.edits.history.push(Undo::Insert);
        self.row = self.total_rows() - 1;
    }

    /// Reverts the most recent staged change. Returns false if there was none.
    pub fn undo(&mut self) -> bool {
        let Some(op) = self.edits.history.pop() else { return false };
        match op {
            Undo::Cell { row, col, prev } => {
                if let Some(i) = row.checked_sub(self.rows.len()) {
                    // For new rows `prev` is the previous cell content
                    // (`None` = default).
                    if let Some(cell) = self.edits.inserted.get_mut(i).and_then(|r| r.get_mut(col)) {
                        *cell = prev;
                    }
                } else {
                    match prev {
                        Some(v) => {
                            self.edits.updates.insert((row, col), v);
                        }
                        None => {
                            self.edits.updates.remove(&(row, col));
                        }
                    }
                }
                self.row = row;
                self.col = col;
            }
            Undo::Delete(row) => {
                if !self.edits.deleted.remove(&row) {
                    self.edits.deleted.insert(row);
                }
                self.row = row;
            }
            Undo::Insert => {
                self.edits.inserted.pop();
                self.row = self.row.min(self.total_rows().saturating_sub(1));
            }
            Undo::RemoveInserted { index, values } => {
                self.edits.inserted.insert(index, values);
                self.row = self.rows.len() + index;
            }
        }
        true
    }

    /// Generates the SQL that applies all staged edits.
    pub fn build_statements(
        &self,
        driver: Driver,
        table: &TableRef,
        structure: &TableStructure,
    ) -> Result<Vec<Statement>, String> {
        let column_info = |grid_col: usize| {
            let name = self.columns.get(grid_col).ok_or("column not found")?;
            structure
                .columns
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| format!("column {name} not found in table structure"))
        };
        let key_cols: Vec<usize> = structure
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .filter_map(|c| self.columns.iter().position(|n| *n == c.name))
            .collect();
        let pk_len = structure.columns.iter().filter(|c| c.primary_key).count();
        let needs_key = !self.edits.updates.is_empty() || !self.edits.deleted.is_empty();
        if needs_key && (pk_len == 0 || key_cols.len() != pk_len) {
            return Err(format!("{table} has no primary key; existing rows cannot be changed safely"));
        }

        // The key as read, so that it matches the row it was read from and no
        // other: never text that merely looks like it (`0x…` for bytes).
        let key_for = |row: usize| -> Result<Vec<_>, String> {
            key_cols
                .iter()
                .map(|&c| {
                    let info = column_info(c)?;
                    let text = self.rows.get(row).and_then(|r| r.get(c)).ok_or("row not found")?;
                    let value = match (text, self.kind(row, c)) {
                        (_, CellKind::Inexact) => None,
                        (None, _) => return Ok((info, None)),
                        (Some(text), kind) => Value::from_cell(text, kind),
                    };
                    match value {
                        Some(value) => Ok((info, Some(value))),
                        None => Err(format!(
                            "The key {} = {} of a row cannot be matched exactly: it is not shown as stored \
                             (binary data or text that is not UTF-8). Change this row with SQL instead.",
                            info.name,
                            text.as_deref().unwrap_or("NULL")
                        )),
                    }
                })
                .collect()
        };

        let mut out = Vec::new();
        let mut by_row: BTreeMap<usize, Vec<(usize, Option<String>)>> = BTreeMap::new();
        for ((row, col), value) in &self.edits.updates {
            if !self.edits.deleted.contains(row) {
                by_row.entry(*row).or_default().push((*col, value.clone()));
            }
        }
        for (row, cols) in by_row {
            let set = cols.into_iter().map(|(c, v)| Ok((column_info(c)?, v))).collect::<Result<Vec<_>, String>>()?;
            out.push(sql::update_row(driver, table, &key_for(row)?, &set));
        }
        for &row in &self.edits.deleted {
            out.push(sql::delete_row(driver, table, &key_for(row)?));
        }
        for new_row in &self.edits.inserted {
            let values = new_row
                .iter()
                .enumerate()
                .filter_map(|(c, v)| v.clone().map(|v| (c, v)))
                .map(|(c, v)| Ok((column_info(c)?, v)))
                .collect::<Result<Vec<_>, String>>()?;
            out.push(sql::insert_row(driver, table, &values));
        }
        Ok(out)
    }
}

pub fn display_width(s: &str) -> usize {
    s.lines().next().map(|l| l.width()).unwrap_or(0) + usize::from(s.contains('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ColumnInfo;

    fn structure() -> TableStructure {
        let col = |name: &str, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: "text".into(),
            nullable: true,
            primary_key: pk,
            ..Default::default()
        };
        TableStructure { columns: vec![col("id", true), col("name", false), col("note", false)], ..Default::default() }
    }

    fn grid() -> Grid {
        Grid::new(
            vec!["id".into(), "name".into(), "note".into()],
            vec![
                vec![Some("1".into()), Some("Ada".into()), None],
                vec![Some("2".into()), Some("Alan".into()), Some("x".into())],
            ],
        )
    }

    #[test]
    fn staged_changes_become_statements() {
        let mut g = grid();
        g.set_cell(0, 1, Some("Ada L.".into()));
        g.set_cell(0, 2, Some("first".into()));
        g.toggle_delete(1);
        g.insert_row();
        let new_row = g.total_rows() - 1;
        g.set_cell(new_row, 1, Some("Grace".into()));
        assert_eq!(g.pending_count(), 3);
        assert_eq!(g.cell(0, 1), (CellValue::Value("Ada L."), CellState::Edited));
        assert_eq!(g.cell(1, 0).1, CellState::Deleted);
        assert_eq!(g.cell(new_row, 2), (CellValue::Default, CellState::Inserted));

        let st = g.build_statements(Driver::Sqlite, &TableRef::new("main", "t"), &structure()).unwrap();
        let sql: Vec<&str> = st.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(
            sql,
            vec![
                "UPDATE \"t\" SET \"name\" = ?, \"note\" = ? WHERE \"id\" = ?",
                "DELETE FROM \"t\" WHERE \"id\" = ?",
                "INSERT INTO \"t\" (\"name\") VALUES (?)",
            ]
        );
        assert_eq!(st[0].params, vec![Some("Ada L.".into()), Some("first".into()), Some("1".into())]);
    }

    #[test]
    fn setting_original_value_unstages() {
        let mut g = grid();
        g.set_cell(0, 1, Some("x".into()));
        g.set_cell(0, 1, Some("Ada".into()));
        assert!(!g.has_pending());
    }

    #[test]
    fn undo_reverts_in_order() {
        let mut g = grid();
        g.set_cell(0, 1, Some("one".into()));
        g.set_cell(0, 1, Some("two".into()));
        g.toggle_delete(1);
        g.insert_row();
        g.set_cell(2, 1, None);
        g.set_default(2, 1);
        assert!(g.undo()); // default -> NULL again
        assert_eq!(g.cell(2, 1).0, CellValue::Null);
        assert!(g.undo()); // NULL -> default
        assert!(g.undo()); // drop new row
        assert_eq!(g.total_rows(), 2);
        assert!(g.undo()); // undelete
        assert!(!g.edits.deleted.contains(&1));
        assert!(g.undo());
        assert_eq!(g.cell(0, 1).0, CellValue::Value("one"));
        assert!(g.undo());
        assert_eq!(g.cell(0, 1), (CellValue::Value("Ada"), CellState::Clean));
        assert!(!g.undo());
    }

    #[test]
    fn deleting_a_new_row_removes_it() {
        let mut g = grid();
        g.insert_row();
        g.toggle_delete(2);
        assert_eq!(g.total_rows(), 2);
        assert!(!g.has_pending());
        assert!(g.undo());
        assert_eq!(g.total_rows(), 3);
    }

    #[test]
    fn edits_without_primary_key_are_refused() {
        let mut s = structure();
        s.columns[0].primary_key = false;
        let mut g = grid();
        g.set_cell(0, 1, Some("x".into()));
        assert!(g.build_statements(Driver::Sqlite, &TableRef::new("main", "t"), &s).is_err());
        // Inserts never need a key.
        let mut g = grid();
        g.insert_row();
        assert!(g.build_statements(Driver::Sqlite, &TableRef::new("main", "t"), &s).is_ok());
    }

    /// Audit L8: key values are bound as they were read, and a key that is
    /// not shown exactly as stored is refused.
    #[test]
    fn keys_keep_their_kind() {
        let mut s = structure();
        s.columns[0].data_type = "varbinary(4)".into();
        let mut g = grid().with_kinds(vec![vec![CellKind::Bytes], vec![CellKind::Inexact]]);
        g.rows[0][0] = Some("0xff00".into());
        g.set_cell(0, 1, Some("x".into()));
        let st = g.build_statements(Driver::MySql, &TableRef::new("db", "t"), &s).unwrap();
        assert_eq!(st[0].params, vec![Some("x".into()), Some(Value::Bytes(vec![0xff, 0]))]);
        g.toggle_delete(1);
        let err = g.build_statements(Driver::MySql, &TableRef::new("db", "t"), &s).unwrap_err();
        assert!(err.contains("The key id = 2 of a row cannot be matched exactly"), "{err}");
    }

    /// A row is found again by its key, wherever it is.
    #[test]
    fn rows_are_found_by_key() {
        let g = grid();
        let key = g.row_key(1, &["id".to_string()]).unwrap();
        let mut reloaded = Grid::new(g.columns.clone(), vec![g.rows[1].clone(), g.rows[0].clone()]);
        assert_eq!(reloaded.find_row(&key), Some(0));
        // Not text that merely looks the same.
        reloaded.kinds = vec![vec![CellKind::Integer]];
        assert_eq!(reloaded.find_row(&key), None);
        reloaded.kinds.clear();
        reloaded.rows.remove(0);
        assert_eq!(reloaded.find_row(&key), None);
        assert!(g.row_key(0, &["missing".to_string()]).is_none());
    }

    /// Only what was committed is cleared; changes staged since stay.
    #[test]
    fn committed_changes_are_removed() {
        let mut g = grid();
        g.set_cell(0, 1, Some("Ada L.".into()));
        g.toggle_delete(1);
        g.insert_row();
        let committed = g.edits.clone();
        let id = g.id;
        g.set_cell(0, 2, Some("note".into()));
        g.set_cell(0, 1, Some("Ada B.".into()));
        g.remove_committed(&committed);
        assert_ne!(g.id, id);
        assert_eq!(g.edits.updates, [((0, 1), Some("Ada B.".to_string())), ((0, 2), Some("note".to_string()))].into());
        assert!(g.edits.deleted.is_empty() && g.edits.inserted.is_empty());
        assert!(!g.undo(), "committed changes cannot be undone into the grid again");
    }

    #[test]
    fn cursor_is_clamped() {
        let mut g = grid();
        g.move_row(10);
        g.move_col(10);
        assert_eq!((g.row, g.col), (1, 2));
        g.move_row(-10);
        assert_eq!(g.row, 0);
        let mut empty = Grid::new(vec![], vec![]);
        empty.move_row(1);
        empty.page(1.0);
        assert_eq!(empty.row, 0);
    }
}
