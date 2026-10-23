//! State of a live connection: schema tree, open table and SQL editor.

use std::collections::HashSet;

use super::grid::{Edits, Grid};
use super::query::QueryState;
use crate::config::ConnectionProfile;
use crate::db::{AccessLevel, Database, Privileges, SchemaInfo, SessionInfo, TableInfo, TableRef, TableStructure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Data,
    Structure,
    Query,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Data, Tab::Structure, Tab::Query];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Data => "Data",
            Tab::Structure => "Structure",
            Tab::Query => "SQL",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeItem {
    Schema(usize),
    Table(usize, usize),
}

pub struct OpenTable {
    /// Table metadata with the *effective* privileges for this session.
    pub info: TableInfo,
    pub structure: Option<TableStructure>,
    pub grid: Grid,
    pub page: usize,
    pub page_size: usize,
    pub filter: Option<String>,
    pub order_by: Option<(String, bool)>,
    pub total: Option<u64>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub structure_scroll: u16,
}

impl OpenTable {
    pub fn new(info: TableInfo, page_size: usize) -> Self {
        Self {
            info,
            structure: None,
            grid: Grid::default(),
            page: 0,
            page_size,
            filter: None,
            order_by: None,
            total: None,
            loading: true,
            loaded: false,
            error: None,
            structure_scroll: 0,
        }
    }

    pub fn has_primary_key(&self) -> bool {
        self.structure.as_ref().is_some_and(|s| s.columns.iter().any(|c| c.primary_key))
    }

    /// Names of the primary-key columns.
    pub fn key_columns(&self) -> Vec<String> {
        let columns = self.structure.as_ref().map(|s| s.columns.as_slice()).unwrap_or_default();
        columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
    }

    pub fn privileges(&self) -> Privileges {
        self.info.privileges
    }

    pub fn can_write(&self) -> bool {
        self.info.privileges.can_write()
    }

    pub fn first_row_number(&self) -> usize {
        self.page * self.page_size
    }

    pub fn has_next_page(&self) -> bool {
        match self.total {
            Some(total) => ((self.page + 1) * self.page_size) < total as usize,
            None => self.grid.rows.len() >= self.page_size,
        }
    }
}

/// The staged changes of a table as a commit preview showed them: they are
/// only committed to that connection and table, as long as they are
/// unchanged, and once committed exactly these are cleared.
#[derive(Debug, Clone)]
pub struct CommitSnapshot {
    /// [`Browser::generation`] of the connection.
    pub generation: u64,
    pub table: TableRef,
    /// [`Grid::id`] of the grid the changes were staged on.
    pub grid: u64,
    pub edits: Edits,
}

/// A commit sent to the database whose result has not come back yet.
pub struct Committing {
    pub seq: u64,
    pub snapshot: CommitSnapshot,
}

pub struct Browser {
    /// Identifies this connection (it is the id of the attempt that opened
    /// it): results of requests made on an earlier one are dropped.
    pub generation: u64,
    pub profile: ConnectionProfile,
    pub db: Database,
    pub session: SessionInfo,
    pub schemas: Vec<SchemaInfo>,
    pub expanded: HashSet<String>,
    pub tree_selected: usize,
    pub tree_offset: usize,
    pub tree_filter: String,
    pub focus: Focus,
    pub tab: Tab,
    pub table: Option<OpenTable>,
    pub query: QueryState,
    /// Latest page load of the open table.
    pub seq: u64,
    /// Latest refresh of the schema tree.
    pub schema_seq: u64,
    pub commit_seq: u64,
    /// The commit in progress: while it runs, the table cannot be changed,
    /// reloaded or swapped for another one.
    pub commit: Option<Committing>,
}

impl Browser {
    pub fn new(
        generation: u64,
        profile: ConnectionProfile,
        db: Database,
        session: SessionInfo,
        schemas: Vec<SchemaInfo>,
    ) -> Self {
        let mut b = Browser {
            generation,
            profile,
            db,
            session,
            schemas,
            expanded: HashSet::new(),
            tree_selected: 0,
            tree_offset: 0,
            tree_filter: String::new(),
            focus: Focus::Sidebar,
            tab: Tab::Data,
            table: None,
            query: QueryState::default(),
            seq: 0,
            schema_seq: 0,
            commit_seq: 0,
            commit: None,
        };
        b.expand_default_schema();
        b
    }

    fn expand_default_schema(&mut self) {
        let preferred = [self.session.database.as_str(), "public", "main"];
        let schema = preferred
            .iter()
            .find_map(|p| self.schemas.iter().position(|s| s.name == *p))
            .or_else(|| self.schemas.iter().position(|s| !s.tables.is_empty()));
        if let Some(idx) = schema {
            self.expanded.insert(self.schemas[idx].name.clone());
            let items = self.tree_items();
            self.tree_selected = items
                .iter()
                .position(|i| matches!(i, TreeItem::Table(s, _) if *s == idx))
                .or_else(|| items.iter().position(|i| *i == TreeItem::Schema(idx)))
                .unwrap_or(0);
        }
    }

    /// Replace the schema list (after a refresh) keeping the selection.
    pub fn set_schemas(&mut self, schemas: Vec<SchemaInfo>) {
        let selected = self.selected_item().map(|i| self.item_key(i));
        self.schemas = schemas;
        if let Some(key) = selected {
            let items = self.tree_items();
            if let Some(pos) = items.iter().position(|i| self.item_key(*i) == key) {
                self.tree_selected = pos;
            }
        }
        self.clamp_tree();
    }

    fn item_key(&self, item: TreeItem) -> (String, Option<String>) {
        match item {
            TreeItem::Schema(s) => (self.schemas[s].name.clone(), None),
            TreeItem::Table(s, t) => (self.schemas[s].name.clone(), Some(self.schemas[s].tables[t].table.name.clone())),
        }
    }

    pub fn session_read_only(&self) -> bool {
        self.session.read_only_reason.is_some()
    }

    /// Privileges after applying session-wide restrictions.
    pub fn effective(&self, privileges: Privileges) -> Privileges {
        if self.session_read_only() { privileges.read_only() } else { privileges }
    }

    pub fn access_level(&self) -> AccessLevel {
        if self.session_read_only() {
            AccessLevel::ReadOnly
        } else if self.session.superuser {
            AccessLevel::Superuser
        } else if self.schemas.iter().flat_map(|s| &s.tables).any(|t| t.privileges.can_write()) {
            AccessLevel::ReadWrite
        } else {
            AccessLevel::ReadOnly
        }
    }

    /// True when the user cannot change anything at all; the SQL editor then
    /// refuses statements that write.
    pub fn fully_read_only(&self) -> bool {
        self.access_level() == AccessLevel::ReadOnly
    }

    pub fn tree_items(&self) -> Vec<TreeItem> {
        let filter = self.tree_filter.to_lowercase();
        let mut out = Vec::new();
        for (si, schema) in self.schemas.iter().enumerate() {
            let matching: Vec<usize> = schema
                .tables
                .iter()
                .enumerate()
                .filter(|(_, t)| filter.is_empty() || t.table.name.to_lowercase().contains(&filter))
                .map(|(i, _)| i)
                .collect();
            if !filter.is_empty() && matching.is_empty() {
                continue;
            }
            out.push(TreeItem::Schema(si));
            if !filter.is_empty() || self.expanded.contains(&schema.name) {
                out.extend(matching.into_iter().map(|ti| TreeItem::Table(si, ti)));
            }
        }
        out
    }

    pub fn selected_item(&self) -> Option<TreeItem> {
        self.tree_items().get(self.tree_selected).copied()
    }

    pub fn clamp_tree(&mut self) {
        let len = self.tree_items().len();
        self.tree_selected = self.tree_selected.min(len.saturating_sub(1));
    }

    pub fn move_tree(&mut self, delta: isize) {
        let len = self.tree_items().len() as isize;
        if len == 0 {
            return;
        }
        self.tree_selected = (self.tree_selected as isize + delta).clamp(0, len - 1) as usize;
    }

    pub fn set_expanded(&mut self, schema: usize, expanded: bool) {
        let name = self.schemas[schema].name.clone();
        if expanded {
            self.expanded.insert(name);
        } else {
            self.expanded.remove(&name);
        }
        if let Some(pos) = self.tree_items().iter().position(|i| *i == TreeItem::Schema(schema)) {
            self.tree_selected = pos;
        }
    }

    pub fn has_pending_edits(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.grid.has_pending())
    }

    pub fn pending_count(&self) -> usize {
        self.table.as_ref().map(|t| t.grid.pending_count()).unwrap_or(0)
    }
}
