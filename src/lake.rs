//! Navigation state for browsing a DuckLake catalog: the table list, the
//! per-table file list, and which slice of the lake the data viewer is showing.

use std::collections::HashMap;

use ratatui::layout::Constraint;

use crate::data::catalog::{self, Catalog, TableInfo};
use crate::ui::BrowserState;

/// Which list the browser is currently showing.
pub enum Level {
    Tables,
    Files { table: usize },
    Snapshots,
}

/// What the data viewer is currently scanning: a whole table, or one file of it.
#[derive(Clone, Copy)]
pub struct Scope {
    pub table: usize,
    /// `None` = every file of the table.
    pub file: Option<usize>,
}

pub struct Lake {
    pub catalog: Catalog,
    pub level: Level,
    pub state: BrowserState,
    pub scope: Option<Scope>,
}

impl Lake {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            level: Level::Tables,
            state: BrowserState::default(),
            scope: None,
        }
    }

    pub fn table(&self, index: usize) -> Option<&TableInfo> {
        self.catalog.tables.get(index)
    }

    /// Number of entries in the list currently being browsed.
    pub fn list_len(&self) -> usize {
        match self.level {
            Level::Tables => self.catalog.tables.len(),
            Level::Files { table } => self.table(table).map_or(0, |t| t.files.len()),
            Level::Snapshots => self.catalog.snapshots.len(),
        }
    }

    /// Index of the currently loaded snapshot in `catalog.snapshots`.
    pub fn current_snapshot_index(&self) -> usize {
        self.catalog
            .snapshots
            .iter()
            .position(|s| s.id == self.catalog.snapshot)
            .unwrap_or(0)
    }

    pub fn title(&self) -> String {
        let lake_name = self
            .catalog
            .path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("lake")
            .to_string();

        match self.level {
            Level::Tables => format!(
                " {lake_name} — {} @ snapshot {} ",
                plural(self.catalog.tables.len(), "table"),
                self.catalog.snapshot
            ),
            Level::Files { table } => match self.table(table) {
                Some(t) => format!(
                    " {} — {}, {} rows ",
                    t.qualified_name(),
                    plural(t.files.len(), "file"),
                    catalog::human_count(t.record_count)
                ),
                None => format!(" {lake_name} "),
            },
            Level::Snapshots => format!(
                " {lake_name} — {}, currently at {} ",
                plural(self.catalog.snapshots.len(), "snapshot"),
                self.catalog.snapshot
            ),
        }
    }

    pub fn headers(&self) -> &'static [&'static str] {
        match self.level {
            Level::Tables => &["Table", "Rows", "Size", "Files", "Partitioned by", "Inlined"],
            Level::Files { .. } => &["File", "Rows", "Size", "Share"],
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
                Constraint::Length(9),
            ],
            Level::Files { .. } => &[
                Constraint::Min(30),
                Constraint::Length(16),
                Constraint::Length(10),
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
                .catalog
                .tables
                .iter()
                .map(|t| {
                    vec![
                        t.qualified_name(),
                        catalog::human_count(t.record_count),
                        catalog::human_bytes(t.file_size),
                        t.files.len().to_string(),
                        if t.partition_cols.is_empty() {
                            "—".to_string()
                        } else {
                            t.partition_cols.join(", ")
                        },
                        if t.inlined_rows == 0 {
                            "—".to_string()
                        } else {
                            catalog::human_count(t.inlined_rows)
                        },
                    ]
                })
                .collect(),
            Level::Files { table } => {
                let Some(t) = self.table(table) else {
                    return Vec::new();
                };
                // Share is of the Parquet rows, which is what the file list
                // actually accounts for — inlined rows have no file.
                let total = t.files.iter().map(|f| f.record_count).sum::<u64>().max(1) as f64;
                // A partition can be spread over several files, so the
                // partition value alone is not a unique label — tag repeats
                // with the catalog's file id.
                let mut seen: HashMap<String, usize> = HashMap::new();
                for f in &t.files {
                    *seen.entry(f.label()).or_default() += 1;
                }
                t.files
                    .iter()
                    .map(|f| {
                        let label = f.label();
                        let label = if seen.get(&label).copied().unwrap_or(0) > 1 {
                            format!("{label}  ·file {}", f.id)
                        } else {
                            label
                        };
                        vec![
                            label,
                            catalog::human_count(f.record_count),
                            catalog::human_bytes(f.file_size),
                            format!("{:.1}%", f.record_count as f64 / total * 100.0),
                        ]
                    })
                    .collect()
            }
            Level::Snapshots => self
                .catalog
                .snapshots
                .iter()
                .map(|s| {
                    vec![
                        if s.id == self.catalog.snapshot { "▸" } else { " " }.to_string(),
                        s.id.to_string(),
                        s.short_time(),
                        format!("v{}", s.schema_version),
                        if s.changes.is_empty() {
                            "—".to_string()
                        } else {
                            s.changes.replace(',', ", ")
                        },
                    ]
                })
                .collect(),
        }
    }

    /// Label for the status bar describing what the viewer is scanning.
    pub fn scope_label(&self) -> Option<String> {
        let scope = self.scope?;
        let table = self.table(scope.table)?;
        let base = format!("{} @snap{}", table.qualified_name(), self.catalog.snapshot);
        Some(match scope.file.and_then(|i| table.files.get(i)) {
            Some(file) => format!("{base} [{}]", file.label()),
            None => format!("{base} [{} files]", table.files.len()),
        })
    }

    /// Warning to show when the current scope hides rows that live in the
    /// catalog database rather than in Parquet.
    pub fn inlined_warning(&self) -> Option<String> {
        let scope = self.scope?;
        let table = self.table(scope.table)?;
        if table.inlined_rows == 0 {
            return None;
        }
        Some(format!(
            "{} row(s) are inlined in the catalog and not shown (Parquet-only scan)",
            catalog::human_count(table.inlined_rows)
        ))
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
        assert_eq!(plural(22, "file"), "22 files");
    }
}
