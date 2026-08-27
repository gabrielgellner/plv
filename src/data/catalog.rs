//! DuckLake catalog reader.
//!
//! A DuckLake "lake" is a set of Parquet files plus a relational catalog that
//! describes them. This module reads that catalog directly (it is an ordinary
//! DuckDB database) and turns it into plain Rust structs, then hands the
//! resolved file paths to Polars for scanning. Polars stays the only query
//! engine — DuckDB is used purely to read metadata.
//!
//! Everything is resolved *as of a snapshot*: catalog rows carry
//! `begin_snapshot`/`end_snapshot` validity ranges, so selecting a snapshot id
//! and filtering on it gives a consistent point-in-time view (time travel).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use duckdb::{AccessMode, Config, Connection, params};
use polars::prelude::*;

/// A committed state of the lake.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: i64,
    pub time: String,
    pub schema_version: i64,
    /// Human-readable summary from `ducklake_snapshot_changes`, e.g.
    /// `inserted_into_table:2,inserted_into_table:4`.
    pub changes: String,
}

#[derive(Clone, Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: String,
    pub nullable: bool,
}

/// One Parquet file backing a table.
#[derive(Clone, Debug)]
pub struct DataFile {
    pub id: i64,
    /// Absolute path on disk, resolved against the lake's data root.
    pub path: PathBuf,
    pub record_count: u64,
    pub file_size: u64,
    /// Hive partition values for this file as `(column_name, value)`, in
    /// partition-key order. Empty for unpartitioned tables.
    pub partition: Vec<(String, String)>,
}

impl DataFile {
    /// Short label for the file pane: the partition value if partitioned,
    /// otherwise the bare file name.
    pub fn label(&self) -> String {
        if self.partition.is_empty() {
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<file>")
                .to_string()
        } else {
            self.partition
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("/")
        }
    }
}

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub table_id: i64,
    pub schema_name: String,
    pub name: String,
    pub record_count: u64,
    pub file_size: u64,
    pub partition_cols: Vec<String>,
    pub columns: Vec<ColumnInfo>,
    pub files: Vec<DataFile>,
    /// Rows held in the catalog database itself rather than in Parquet
    /// (DuckDB's data inlining). These are invisible to a plain Parquet scan.
    pub inlined_rows: u64,
}

impl TableInfo {
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.schema_name, self.name)
    }
}

pub struct Catalog {
    pub path: PathBuf,
    pub data_root: PathBuf,
    /// Snapshot the `tables` below were resolved at.
    pub snapshot: i64,
    pub snapshots: Vec<Snapshot>,
    pub tables: Vec<TableInfo>,
}

/// True if `path` looks like something we should open as a lake: a
/// `.ducklake` catalog file, or a directory containing one.
pub fn detect(path: &Path) -> Option<PathBuf> {
    if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("ducklake") {
        return Some(path.to_path_buf());
    }
    if path.is_dir() {
        let entries = std::fs::read_dir(path).ok()?;
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ducklake"))
            .collect();
        found.sort();
        return found.into_iter().next();
    }
    None
}

impl Catalog {
    /// Open a catalog and resolve its newest snapshot.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_at(path, None)
    }

    /// Open a catalog and resolve it as of `snapshot` (newest if `None`).
    pub fn open_at(path: &Path, snapshot: Option<i64>) -> Result<Self> {
        // Read-only matters twice over: a viewer must never mutate a lake, and
        // DuckDB takes an exclusive lock on a read-write handle — which would
        // stop a second plv (or any other reader) from opening the same bundle.
        let config = Config::default().access_mode(AccessMode::ReadOnly)?;
        let conn = Connection::open_with_flags(path, config).map_err(|e| {
            // A writer (e.g. an ingest job) holds an exclusive lock on the
            // catalog. That is the common failure when viewing a lake that is
            // still being built, so say so rather than leaking the raw IO error.
            if e.to_string().contains("Conflicting lock") {
                anyhow::anyhow!(
                    "{} is locked by another process that has it open for writing",
                    path.display()
                )
            } else {
                anyhow::Error::new(e)
                    .context(format!("opening DuckLake catalog {}", path.display()))
            }
        })?;

        let data_root = resolve_data_root(path, &declared_data_path(&conn)?);
        let snapshots = read_snapshots(&conn)?;

        let latest = snapshots.last().map(|s| s.id).unwrap_or(0);
        let snapshot = snapshot.unwrap_or(latest);

        let tables = read_tables(&conn, snapshot, &data_root)?;

        Ok(Self {
            path: path.to_path_buf(),
            data_root,
            snapshot,
            snapshots,
            tables,
        })
    }

    /// Lazy frame over every file of `table` (plus nothing else — inlined rows
    /// are reported but not merged; see `TableInfo::inlined_rows`).
    pub fn scan_table(&self, table: &TableInfo) -> Result<LazyFrame> {
        self.scan_files(table, &table.files)
    }

    /// Lazy frame over an explicit subset of a table's files.
    ///
    /// Each file is scanned individually so that Hive partition values taken
    /// from the *catalog* can be attached as literal columns. That avoids
    /// relying on path-shape inference and keeps the column order identical to
    /// the catalog schema across every file.
    pub fn scan_files(&self, table: &TableInfo, files: &[DataFile]) -> Result<LazyFrame> {
        if files.is_empty() {
            anyhow::bail!("table {} has no data files", table.qualified_name());
        }

        let frames: Vec<LazyFrame> = files
            .iter()
            .map(|f| self.scan_one(table, f))
            .collect::<Result<_>>()?;

        if frames.len() == 1 {
            return Ok(frames.into_iter().next().expect("len checked"));
        }
        Ok(concat(frames, UnionArgs::default())?)
    }

    fn scan_one(&self, table: &TableInfo, file: &DataFile) -> Result<LazyFrame> {
        let pl_path = PlRefPath::try_from_path(&file.path)
            .with_context(|| format!("bad data file path {}", file.path.display()))?;
        let mut lf = LazyFrame::scan_parquet(pl_path, Default::default())
            .with_context(|| format!("scanning {}", file.path.display()))?;

        // Partition columns are encoded in the directory path, so they are
        // usually absent from the file itself — but a writer is free to store
        // them too. Only synthesise the ones actually missing.
        let physical = lf.collect_schema()?;
        let missing: Vec<Expr> = file
            .partition
            .iter()
            .filter(|(name, _)| physical.get(name.as_str()).is_none())
            .map(|(name, value)| lit(value.as_str()).alias(name.as_str()))
            .collect();
        if !missing.is_empty() {
            lf = lf.with_columns(missing);
        }

        // Project to catalog column order, skipping any catalog column the file
        // cannot supply (e.g. a column added by a later schema version).
        let available: Vec<Expr> = table
            .columns
            .iter()
            .filter(|c| {
                physical.get(c.name.as_str()).is_some()
                    || file.partition.iter().any(|(n, _)| *n == c.name)
            })
            .map(|c| col(c.name.as_str()))
            .collect();
        if !available.is_empty() {
            lf = lf.select(available);
        }

        Ok(lf)
    }
}

fn declared_data_path(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT value FROM ducklake_metadata WHERE key = 'data_path'")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(row.get::<_, String>(0)?),
        None => Ok(String::new()),
    }
}

/// Locate the Parquet root for a lake.
///
/// `ducklake_metadata.data_path` is written as an absolute path by the machine
/// that built the lake, so it is wrong the moment a bundle is copied elsewhere.
/// Prefer it when it still exists, then fall back to the same-named directory
/// beside the catalog file, then to the catalog's own directory.
fn resolve_data_root(lake_path: &Path, declared: &str) -> PathBuf {
    let dir = lake_path.parent().unwrap_or_else(|| Path::new("."));

    let trimmed = declared.trim_end_matches('/');
    if !trimmed.is_empty() {
        let declared_path = Path::new(trimmed);
        if declared_path.is_dir() {
            return declared_path.to_path_buf();
        }
        if let Some(last) = declared_path.file_name() {
            let candidate = dir.join(last);
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    dir.to_path_buf()
}

/// SQL predicate restricting a catalog table to rows visible at snapshot `?1`.
/// `alias` is the table alias to qualify with (empty for an unaliased table).
fn visible(alias: &str) -> String {
    let p = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    format!("{p}begin_snapshot <= ?1 AND ({p}end_snapshot IS NULL OR {p}end_snapshot > ?1)")
}

fn read_snapshots(conn: &Connection) -> Result<Vec<Snapshot>> {
    let mut stmt = conn.prepare(
        "SELECT s.snapshot_id, s.snapshot_time::VARCHAR, s.schema_version,
                COALESCE(c.changes_made, '')
         FROM ducklake_snapshot s
         LEFT JOIN ducklake_snapshot_changes c USING (snapshot_id)
         ORDER BY s.snapshot_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Snapshot {
            id: row.get(0)?,
            time: row.get(1)?,
            schema_version: row.get(2)?,
            changes: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn read_tables(conn: &Connection, snap: i64, data_root: &Path) -> Result<Vec<TableInfo>> {
    let columns = read_columns(conn, snap)?;
    let partition_cols = read_partition_cols(conn, snap)?;
    let file_partitions = read_file_partitions(conn)?;
    let stats = read_table_stats(conn)?;
    let inlined = read_inlined_counts(conn, snap)?;

    // Table directory = data_root / schema.path / table.path, honouring the
    // path_is_relative flags at each level.
    let mut stmt = conn.prepare(&format!(
        "SELECT t.table_id, s.schema_name, t.table_name,
                s.path, s.path_is_relative, t.path, t.path_is_relative
         FROM ducklake_table t
         JOIN ducklake_schema s ON s.schema_id = t.schema_id
              AND {}
         WHERE {}
         ORDER BY s.schema_name, t.table_name",
        visible("s"),
        visible("t")
    ))?;

    let rows = stmt.query_map(params![snap], |row| {
        let table_id: i64 = row.get(0)?;
        let schema_name: String = row.get(1)?;
        let table_name: String = row.get(2)?;
        let schema_path: String = row.get(3)?;
        let schema_rel: bool = row.get(4)?;
        let table_path: String = row.get(5)?;
        let table_rel: bool = row.get(6)?;
        Ok((
            table_id,
            schema_name,
            table_name,
            join_path(data_root, &schema_path, schema_rel),
            table_path,
            table_rel,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (table_id, schema_name, name, schema_dir, table_path, table_rel) = row?;
        let table_dir = join_path(&schema_dir, &table_path, table_rel);

        let part_cols = partition_cols.get(&table_id).cloned().unwrap_or_default();
        let files = read_files(conn, snap, table_id, &table_dir, &part_cols, &file_partitions)?;
        let (record_count, file_size) = stats.get(&table_id).copied().unwrap_or((0, 0));

        out.push(TableInfo {
            table_id,
            schema_name,
            name,
            record_count,
            file_size,
            partition_cols: part_cols,
            columns: columns.get(&table_id).cloned().unwrap_or_default(),
            files,
            inlined_rows: inlined.get(&table_id).copied().unwrap_or(0),
        });
    }
    Ok(out)
}

fn join_path(base: &Path, path: &str, relative: bool) -> PathBuf {
    let trimmed = path.trim_end_matches('/');
    if relative {
        base.join(trimmed)
    } else {
        PathBuf::from(trimmed)
    }
}

fn read_columns(conn: &Connection, snap: i64) -> Result<HashMap<i64, Vec<ColumnInfo>>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT table_id, column_name, column_type, nulls_allowed
         FROM ducklake_column
         WHERE {} AND parent_column IS NULL
         ORDER BY table_id, column_order",
        visible("")
    ))?;
    let rows = stmt.query_map(params![snap], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            ColumnInfo {
                name: row.get(1)?,
                ty: row.get(2)?,
                nullable: row.get(3)?,
            },
        ))
    })?;

    let mut map: HashMap<i64, Vec<ColumnInfo>> = HashMap::new();
    for row in rows {
        let (table_id, col) = row?;
        map.entry(table_id).or_default().push(col);
    }
    Ok(map)
}

fn read_partition_cols(conn: &Connection, snap: i64) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT pc.table_id, c.column_name
         FROM ducklake_partition_column pc
         JOIN ducklake_partition_info pi ON pi.partition_id = pc.partition_id
              AND {}
         JOIN ducklake_column c ON c.table_id = pc.table_id
              AND c.column_id = pc.column_id AND {}
         ORDER BY pc.table_id, pc.partition_key_index",
        visible("pi"),
        visible("c")
    ))?;
    let rows = stmt.query_map(params![snap], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (table_id, name) = row?;
        map.entry(table_id).or_default().push(name);
    }
    Ok(map)
}

/// `data_file_id -> partition values in key order`.
fn read_file_partitions(conn: &Connection) -> Result<HashMap<i64, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT data_file_id, partition_value
         FROM ducklake_file_partition_value
         ORDER BY data_file_id, partition_key_index",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (file_id, value) = row?;
        map.entry(file_id).or_default().push(value);
    }
    Ok(map)
}

fn read_table_stats(conn: &Connection) -> Result<HashMap<i64, (u64, u64)>> {
    let mut stmt =
        conn.prepare("SELECT table_id, record_count, file_size_bytes FROM ducklake_table_stats")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            (row.get::<_, i64>(1)? as u64, row.get::<_, i64>(2)? as u64),
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn read_files(
    conn: &Connection,
    snap: i64,
    table_id: i64,
    table_dir: &Path,
    partition_cols: &[String],
    file_partitions: &HashMap<i64, Vec<String>>,
) -> Result<Vec<DataFile>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT data_file_id, path, path_is_relative, record_count, file_size_bytes
         FROM ducklake_data_file
         WHERE table_id = ?2 AND {}
         ORDER BY file_order, data_file_id",
        visible("")
    ))?;

    let rows = stmt.query_map(params![snap, table_id], |row| {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        let relative: bool = row.get(2)?;
        Ok(DataFile {
            id,
            path: join_path(table_dir, &path, relative),
            record_count: row.get::<_, i64>(3)? as u64,
            file_size: row.get::<_, i64>(4)? as u64,
            partition: Vec::new(),
        })
    })?;

    let mut files: Vec<DataFile> = rows.collect::<std::result::Result<_, _>>()?;
    for file in &mut files {
        if let Some(values) = file_partitions.get(&file.id) {
            file.partition = partition_cols
                .iter()
                .cloned()
                .zip(values.iter().cloned())
                .collect();
        }
    }
    Ok(files)
}

/// Row counts for data DuckDB has inlined into the catalog instead of writing
/// to Parquet. Reported so the UI can warn that a Parquet-only scan is partial.
fn read_inlined_counts(conn: &Connection, snap: i64) -> Result<HashMap<i64, u64>> {
    let mut stmt =
        conn.prepare("SELECT table_id, table_name FROM ducklake_inlined_data_tables")?;
    let listed = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map: HashMap<i64, u64> = HashMap::new();
    for row in listed {
        let (table_id, name) = row?;
        // Name comes from the catalog and is interpolated into SQL, so accept
        // only the documented shape.
        if !name
            .strip_prefix("ducklake_inlined_data_")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'_'))
        {
            continue;
        }
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{name}\" WHERE {}", visible("")),
            params![snap],
            |row| row.get(0),
        )?;
        *map.entry(table_id).or_default() += count as u64;
    }
    Ok(map)
}

/// Compact human-readable byte size, e.g. `839.0 MB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Thousands-separated row count, e.g. `1,140,560,738`.
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(839_049_956), "800.2 MB");
    }

    #[test]
    fn human_count_groups_digits() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_140_560_738), "1,140,560,738");
    }

    #[test]
    fn join_path_respects_absolute() {
        let base = Path::new("/lake");
        assert_eq!(join_path(base, "obs/", true), PathBuf::from("/lake/obs"));
        assert_eq!(join_path(base, "/data/obs", false), PathBuf::from("/data/obs"));
    }

    #[test]
    fn data_root_falls_back_beside_catalog() {
        // A declared path that does not exist must not be trusted.
        let lake = Path::new("/nonexistent/bundle/lake.ducklake");
        let root = resolve_data_root(lake, "/some/other/machine/parquet/");
        assert_eq!(root, PathBuf::from("/nonexistent/bundle"));
    }
}
