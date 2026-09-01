//! Navigation state for browsing a DuckLake lake: the table list, the
//! per-table partition list, the snapshot list, and which slice the viewer is
//! currently showing.

use ratatui::layout::Constraint;

use crate::data::lake_db::{self, LakeDb, Partition, Snapshot, TableInfo};
use crate::ui::BrowserState;

/// Which list the browser is currently showing.
pub enum Level {
    Tables,
    Partitions { table: usize },
    Snapshots,
}

/// What the data viewer is currently scanning: a whole table, or one partition.
#[derive(Clone, Copy)]
pub struct Scope {
    pub table: usize,
    /// `None` = the whole table.
    pub partition: Option<usize>,
}

pub struct Lake {
    pub db: LakeDb,
    pub snapshot: i64,
    pub snapshots: Vec<Snapshot>,
    pub tables: Vec<TableInfo>,
    /// Partitions of the table currently being browsed. Loaded on demand,
    /// because each list costs a GROUP BY over the table.
    pub partitions: Vec<Partition>,
    partitions_of: Option<usize>,
    pub level: Level,
    pub state: BrowserState,
    pub scope: Option<Scope>,
}

impl Lake {
    pub fn new(db: LakeDb) -> anyhow::Result<Self> {
        let snapshot = db.current_snapshot()?;
        let snapshots = db.snapshots()?;
        let tables = db.tables()?;
        Ok(Self {
            db,
            snapshot,
            snapshots,
            tables,
            partitions: Vec::new(),
            partitions_of: None,
            level: Level::Tables,
            state: BrowserState::default(),
            scope: None,
        })
    }

    pub fn table(&self, index: usize) -> Option<&TableInfo> {
        self.tables.get(index)
    }

    /// Load the partition list for `table` unless it is already loaded.
    pub fn load_partitions(&mut self, table: usize) -> anyhow::Result<()> {
        if self.partitions_of == Some(table) {
            return Ok(());
        }
        let Some(info) = self.tables.get(table) else {
            return Ok(());
        };
        self.partitions = self.db.partitions(info)?;
        self.partitions_of = Some(table);
        Ok(())
    }

    /// Number of entries in the list currently being browsed.
    pub fn list_len(&self) -> usize {
        match self.level {
            Level::Tables => self.tables.len(),
            Level::Partitions { .. } => self.partitions.len(),
            Level::Snapshots => self.snapshots.len(),
        }
    }

    /// Index of the currently loaded snapshot in `snapshots`.
    pub fn current_snapshot_index(&self) -> usize {
        self.snapshots
            .iter()
            .position(|s| s.id == self.snapshot)
            .unwrap_or(0)
    }

    fn lake_name(&self) -> String {
        self.db
            .path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("lake")
            .to_string()
    }

    pub fn title(&self) -> String {
        match self.level {
            Level::Tables => format!(
                " {} — {} @ snapshot {} ",
                self.lake_name(),
                plural(self.tables.len(), "table"),
                self.snapshot
            ),
            Level::Partitions { table } => match self.table(table) {
                Some(t) => format!(
                    " {} — {}, {} rows ",
                    t.qualified_name(),
                    plural(self.partitions.len(), "partition"),
                    lake_db::human_count(t.rows)
                ),
                None => format!(" {} ", self.lake_name()),
            },
            Level::Snapshots => format!(
                " {} — {}, currently at {} ",
                self.lake_name(),
                plural(self.snapshots.len(), "snapshot"),
                self.snapshot
            ),
        }
    }

    pub fn headers(&self) -> &'static [&'static str] {
        match self.level {
            Level::Tables => &[
                "Table",
                "Rows",
                "Size",
                "Files",
                "Partitioned by",
                "Deletes",
            ],
            Level::Partitions { .. } => &["Partition", "Rows", "Share"],
            Level::Snapshots => &["", "Snapshot", "Time", "Schema", "Changes"],
        }
    }

    pub fn widths(&self) -> &'static [Constraint] {
        match self.level {
            Level::Tables => &[
                Constraint::Min(34),
                Constraint::Length(16),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Length(18),
                Constraint::Length(8),
            ],
            Level::Partitions { .. } => &[
                Constraint::Min(30),
                Constraint::Length(16),
                Constraint::Length(7),
            ],
            Level::Snapshots => &[
                Constraint::Length(1),
                Constraint::Length(8),
                Constraint::Length(21),
                Constraint::Length(6),
                Constraint::Min(30),
            ],
        }
    }

    pub fn rows(&self) -> Vec<Vec<String>> {
        match self.level {
            Level::Tables => self
                .tables
                .iter()
                .map(|t| {
                    vec![
                        t.qualified_name(),
                        lake_db::human_count(t.rows),
                        lake_db::human_bytes(t.file_size),
                        t.file_count.to_string(),
                        if t.partition_cols.is_empty() {
                            "—".to_string()
                        } else {
                            t.partition_cols.join(", ")
                        },
                        if t.delete_file_count == 0 {
                            "—".to_string()
                        } else {
                            t.delete_file_count.to_string()
                        },
                    ]
                })
                .collect(),
            Level::Partitions { table } => {
                let total = self.table(table).map_or(1, |t| t.rows).max(1) as f64;
                self.partitions
                    .iter()
                    .map(|p| {
                        vec![
                            p.label(),
                            lake_db::human_count(p.rows),
                            format!("{:.1}%", p.rows as f64 / total * 100.0),
                        ]
                    })
                    .collect()
            }
            Level::Snapshots => self
                .snapshots
                .iter()
                .map(|s| {
                    vec![
                        if s.id == self.snapshot { "▸" } else { " " }.to_string(),
                        s.id.to_string(),
                        s.short_time(),
                        format!("v{}", s.schema_version),
                        // `changes` arrives pre-formatted from the extension.
                        s.commit_message.clone().unwrap_or_else(|| {
                            if s.changes.is_empty() {
                                "—".to_string()
                            } else {
                                s.changes.clone()
                            }
                        }),
                    ]
                })
                .collect(),
        }
    }

    /// Label for the status bar describing what the viewer is scanning.
    pub fn scope_label(&self) -> Option<String> {
        let scope = self.scope?;
        let table = self.table(scope.table)?;
        let base = format!("{} @snap{}", table.qualified_name(), self.snapshot);
        Some(match scope.partition.and_then(|i| self.partitions.get(i)) {
            Some(partition) => format!("{base} [{}]", partition.label()),
            None => base,
        })
    }
}

/// `1 table` / `2 tables`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::plural;

    #[test]
    fn plural_agrees_with_count() {
        assert_eq!(plural(0, "table"), "0 tables");
        assert_eq!(plural(1, "table"), "1 table");
        assert_eq!(plural(22, "partition"), "22 partitions");
    }
}
