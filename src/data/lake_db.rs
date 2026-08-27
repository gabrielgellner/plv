//! DuckLake access through DuckDB's `ducklake` extension.
//!
//! The extension is DuckLake's reference reader: it presents the *logical*
//! table, merging rows that are inlined in the catalog database, applying
//! delete files, and handling schema evolution and column mapping. Reading the
//! Parquet files directly would mean re-implementing all of that against a
//! 28-table spec, so lake data is read through the extension and only CSV and
//! Parquet go through Polars.
//!
//! Time travel is an ATTACH option (`SNAPSHOT_VERSION`), so selecting a
//! snapshot means re-opening the lake rather than rewriting every query.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use duckdb::types::Value;
use duckdb::{Connection, params};
use polars::prelude::*;

/// Alias the lake is attached under. Not user-visible.
const ALIAS: &str = "lake";

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: i64,
    pub time: String,
    pub schema_version: i64,
    /// Summary of what the snapshot changed, e.g. `inserted_into_table:2`.
    pub changes: String,
    pub commit_message: Option<String>,
}

impl Snapshot {
    /// Timestamp trimmed to whole seconds for display.
    pub fn short_time(&self) -> String {
        match self.time.split_once('.') {
            Some((head, _)) => head.to_string(),
            None => self.time.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    /// Logical row count at the resolved snapshot — inlined rows included.
    pub rows: u64,
    pub file_count: u64,
    pub file_size: u64,
    pub delete_file_count: u64,
    /// Hive partition columns, discovered from the data file paths.
    pub partition_cols: Vec<String>,
}

impl TableInfo {
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    /// Fully-qualified, quoted SQL reference to this table.
    fn sql_ref(&self) -> String {
        format!(
            "{}.{}.{}",
            quote_ident(ALIAS),
            quote_ident(&self.schema),
            quote_ident(&self.name)
        )
    }
}

/// One value of a table's partition key, with its logical row count.
#[derive(Clone, Debug)]
pub struct Partition {
    pub values: Vec<(String, String)>,
    pub rows: u64,
}

impl Partition {
    pub fn label(&self) -> String {
        self.values
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Everything needed to query one slice of the lake, cheap to clone so a
/// background thread can take its own copy.
#[derive(Clone, Debug)]
pub struct LakeSource {
    table: String,
    /// SQL predicate restricting to one partition, if any.
    filter: Option<String>,
}

impl LakeSource {
    fn relation(&self) -> String {
        match &self.filter {
            Some(f) => format!("{} WHERE {f}", self.table),
            None => self.table.clone(),
        }
    }

    /// `SELECT * FROM …` with an optional ORDER BY, ready for LIMIT/OFFSET.
    fn ordered(&self, sort: &[(String, bool)]) -> String {
        let mut sql = format!("SELECT * FROM {}", self.relation());
        if !sort.is_empty() {
            let keys: Vec<String> = sort
                .iter()
                .map(|(name, asc)| {
                    format!("{} {}", quote_ident(name), if *asc { "ASC" } else { "DESC" })
                })
                .collect();
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        sql
    }
}

pub struct LakeDb {
    conn: Connection,
    pub path: PathBuf,
    /// Snapshot the attachment is pinned to, or `None` for the newest.
    pub snapshot: Option<i64>,
}

/// True if `path` is something we should open as a lake: a `.ducklake`
/// catalog, or a directory containing one.
pub fn detect(path: &Path) -> Option<PathBuf> {
    if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("ducklake") {
        return Some(path.to_path_buf());
    }
    if path.is_dir() {
        let mut found: Vec<PathBuf> = std::fs::read_dir(path)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ducklake"))
            .collect();
        found.sort();
        return found.into_iter().next();
    }
    None
}

impl LakeDb {
    /// Attach `path` read-only, pinned to `snapshot` if given.
    pub fn open(path: &Path, snapshot: Option<i64>) -> Result<Self> {
        let conn = Connection::open_in_memory().context("starting DuckDB")?;
        conn.execute_batch("INSTALL ducklake; LOAD ducklake;")
            .context(
                "loading the DuckLake extension \
                 (the first lake you open needs network access to fetch it)",
            )?;

        let pin = match snapshot {
            Some(id) => format!(", SNAPSHOT_VERSION {id}"),
            None => String::new(),
        };
        let attach = |options: &str| {
            format!(
                "ATTACH {} AS {} (TYPE DUCKLAKE, READ_ONLY{options}{pin})",
                sql_str(&path.to_string_lossy()),
                quote_ident(ALIAS)
            )
        };

        conn.execute_batch(&attach(""))
            .map_err(|e| attach_error(path, e))?;

        // A lake records the absolute data path of the machine that built it,
        // which is wrong once a bundle is copied elsewhere. Only override it —
        // DuckLake rejects a mismatched DATA_PATH otherwise — when the recorded
        // path is gone and the files are sitting next to the catalog instead.
        if let Some(local) = relocated_data_path(&conn, path) {
            conn.execute_batch(&format!("DETACH {}", quote_ident(ALIAS)))?;
            conn.execute_batch(&attach(&format!(
                ", DATA_PATH {}, OVERRIDE_DATA_PATH true",
                sql_str(&local)
            )))
            .map_err(|e| attach_error(path, e))?;
        }

        Ok(Self {
            conn,
            path: path.to_path_buf(),
            snapshot,
        })
    }

    /// A second handle on the same attached lake, for a background thread.
    /// Sharing the attachment avoids paying the ~20ms ATTACH cost again.
    pub fn try_clone(&self) -> Result<Connection> {
        Ok(self.conn.try_clone()?)
    }

    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT snapshot_id, snapshot_time::VARCHAR, schema_version,
                    COALESCE(changes::VARCHAR, ''), commit_message
             FROM ducklake_snapshots({})
             ORDER BY snapshot_id",
            sql_str(ALIAS)
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(Snapshot {
                id: row.get(0)?,
                time: row.get(1)?,
                schema_version: row.get(2)?,
                changes: row.get(3)?,
                commit_message: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Snapshot the lake is currently resolved at.
    pub fn current_snapshot(&self) -> Result<i64> {
        if let Some(id) = self.snapshot {
            return Ok(id);
        }
        Ok(self.conn.query_row(
            &format!("SELECT id FROM ducklake_current_snapshot({})", sql_str(ALIAS)),
            [],
            |row| row.get(0),
        )?)
    }

    pub fn tables(&self) -> Result<Vec<TableInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT schema_name, table_name FROM duckdb_tables()
             WHERE database_name = ?1 ORDER BY schema_name, table_name",
        )?;
        let listed = stmt.query_map(params![ALIAS], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut out = Vec::new();
        for row in listed {
            let (schema, name) = row?;
            let mut table = TableInfo {
                schema,
                name,
                rows: 0,
                file_count: 0,
                file_size: 0,
                delete_file_count: 0,
                partition_cols: Vec::new(),
            };

            // Catalog-backed, so this is a metadata lookup rather than a scan
            // even on a billion-row table.
            table.rows = self
                .conn
                .query_row(&format!("SELECT count(*) FROM {}", table.sql_ref()), [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0)
                .max(0) as u64;

            if let Ok((files, size, deletes)) = self.file_stats(&table) {
                table.file_count = files;
                table.file_size = size;
                table.delete_file_count = deletes;
            }
            table.partition_cols = self.partition_cols(&table).unwrap_or_default();
            out.push(table);
        }
        Ok(out)
    }

    fn file_stats(&self, table: &TableInfo) -> Result<(u64, u64, u64)> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT file_count, file_size_bytes, delete_file_count
                 FROM ducklake_table_info({}) WHERE table_name = ?1",
                sql_str(ALIAS)
            ),
            params![table.name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?)
    }

    /// Partition columns, read off the Hive-style data file paths.
    ///
    /// The logical reader deliberately hides physical layout, so it exposes no
    /// partition metadata. The paths are enough to learn *which* columns the
    /// table is partitioned on; the values shown to the user come from an
    /// actual GROUP BY, not from parsing.
    fn partition_cols(&self, table: &TableInfo) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT data_file FROM ducklake_list_files({}, {}, schema => {}) LIMIT 1",
            sql_str(ALIAS),
            sql_str(&table.name),
            sql_str(&table.schema)
        ))?;
        let mut rows = stmt.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(Vec::new());
        };
        let path: String = row.get(0)?;
        Ok(hive_keys(&path))
    }

    /// Partition values of `table` with their logical row counts.
    pub fn partitions(&self, table: &TableInfo) -> Result<Vec<Partition>> {
        if table.partition_cols.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = table.partition_cols.iter().map(|c| quote_ident(c)).collect();
        let sql = format!(
            "SELECT {}, count(*) FROM {} GROUP BY ALL ORDER BY count(*) DESC",
            keys.join(", "),
            table.sql_ref()
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let n = table.partition_cols.len();
        let rows = stmt.query_map([], |row| {
            let mut values = Vec::with_capacity(n);
            for (i, col) in table.partition_cols.iter().enumerate() {
                values.push((col.clone(), row.get::<_, String>(i)?));
            }
            Ok(Partition {
                values,
                rows: row.get::<_, i64>(n)?.max(0) as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// A source for the whole table, or for one partition of it.
    pub fn source(&self, table: &TableInfo, partition: Option<&Partition>) -> LakeSource {
        LakeSource {
            table: table.sql_ref(),
            filter: partition.map(|p| {
                p.values
                    .iter()
                    .map(|(col, value)| format!("{} = {}", quote_ident(col), sql_str(value)))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            }),
        }
    }

    pub fn count(&self, source: &LakeSource) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM {}", source.relation()),
            [],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    pub fn page(
        &self,
        source: &LakeSource,
        sort: &[(String, bool)],
        offset: usize,
        limit: usize,
    ) -> Result<DataFrame> {
        page_with(&self.conn, source, sort, offset, limit)
    }
}

/// Fetch one page as a `DataFrame`. Free function so a background thread can
/// call it with its own cloned connection.
pub fn page_with(
    conn: &Connection,
    source: &LakeSource,
    sort: &[(String, bool)],
    offset: usize,
    limit: usize,
) -> Result<DataFrame> {
    let sql = format!("{} LIMIT {limit} OFFSET {offset}", source.ordered(sort));
    let mut stmt = conn.prepare(&sql)?;
    let names: Vec<String> = {
        let rows = stmt.query([])?;
        rows.as_ref()
            .map(|r| r.column_names())
            .unwrap_or_default()
            .into_iter()
            .collect()
    };

    let mut columns: Vec<Vec<Value>> = vec![Vec::new(); names.len()];
    let mut height = 0usize;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for (i, column) in columns.iter_mut().enumerate() {
            column.push(row.get::<_, Value>(i)?);
        }
        height += 1;
    }

    let series: Vec<Column> = names
        .iter()
        .zip(columns)
        .map(|(name, values)| series_from_values(name, values))
        .collect();
    Ok(DataFrame::new(height, series)?)
}

/// SQL selecting the 0-based row indices of matches within one chunk.
///
/// Numbering happens before filtering so the indices line up with what the
/// viewer displays; the chunk is bounded so a scan of a large table can be
/// cancelled between chunks.
pub fn match_indices_sql(
    source: &LakeSource,
    sort: &[(String, bool)],
    column: Option<&str>,
    pattern: &str,
    offset: usize,
    limit: usize,
    columns: &[String],
) -> String {
    let predicate = match column {
        Some(name) => regex_match(name, pattern),
        None => columns
            .iter()
            .map(|name| regex_match(name, pattern))
            .collect::<Vec<_>>()
            .join(" OR "),
    };
    format!(
        "SELECT idx FROM (
           SELECT {offset} + row_number() OVER () - 1 AS idx, *
           FROM ({} LIMIT {limit} OFFSET {offset}) chunk
         ) numbered WHERE {predicate}",
        source.ordered(sort)
    )
}

/// Column names of the source, in display order.
pub fn column_names(conn: &Connection, source: &LakeSource) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {} LIMIT 0", source.relation()))?;
    let rows = stmt.query([])?;
    Ok(rows
        .as_ref()
        .map(|r| r.column_names())
        .unwrap_or_default()
        .into_iter()
        .collect())
}

fn regex_match(column: &str, pattern: &str) -> String {
    format!(
        "regexp_matches(CAST({} AS VARCHAR), {}, 'i')",
        quote_ident(column),
        sql_str(pattern)
    )
}

/// Build a Polars column from DuckDB values.
///
/// Numeric and boolean columns keep their type so the table renders them as
/// numbers; everything else (text, dates, timestamps, blobs, nested types) is
/// rendered as text, which is what a viewer displays anyway.
fn series_from_values(name: &str, values: Vec<Value>) -> Column {
    let kind = values.iter().find(|v| !matches!(v, Value::Null));

    match kind {
        Some(Value::Boolean(_)) => {
            let data: Vec<Option<bool>> = values
                .iter()
                .map(|v| match v {
                    Value::Boolean(b) => Some(*b),
                    _ => None,
                })
                .collect();
            Column::new(name.into(), data)
        }
        Some(v) if is_integer(v) => {
            let data: Vec<Option<i64>> = values.iter().map(value_as_i64).collect();
            Column::new(name.into(), data)
        }
        Some(v) if is_float(v) => {
            let data: Vec<Option<f64>> = values.iter().map(value_as_f64).collect();
            Column::new(name.into(), data)
        }
        _ => {
            let data: Vec<Option<String>> = values.iter().map(value_as_string).collect();
            Column::new(name.into(), data)
        }
    }
}

fn is_integer(v: &Value) -> bool {
    matches!(
        v,
        Value::TinyInt(_)
            | Value::SmallInt(_)
            | Value::Int(_)
            | Value::BigInt(_)
            | Value::UTinyInt(_)
            | Value::USmallInt(_)
            | Value::UInt(_)
            | Value::UBigInt(_)
    )
}

fn is_float(v: &Value) -> bool {
    matches!(v, Value::Float(_) | Value::Double(_) | Value::Decimal(_))
}

fn value_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::TinyInt(n) => Some(*n as i64),
        Value::SmallInt(n) => Some(*n as i64),
        Value::Int(n) => Some(*n as i64),
        Value::BigInt(n) => Some(*n),
        Value::UTinyInt(n) => Some(*n as i64),
        Value::USmallInt(n) => Some(*n as i64),
        Value::UInt(n) => Some(*n as i64),
        Value::UBigInt(n) => i64::try_from(*n).ok(),
        _ => None,
    }
}

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(n) => Some(*n as f64),
        Value::Double(n) => Some(*n),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => value_as_i64(v).map(|n| n as f64),
    }
}

fn value_as_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        Value::Boolean(b) => Some(b.to_string()),
        Value::Blob(b) => Some(format!("<{} bytes>", b.len())),
        other => Some(format!("{other:?}")),
    }
}

/// `GEO_LEVEL=Country/file.parquet` → `["GEO_LEVEL"]`.
fn hive_keys(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|part| part.split_once('='))
        .map(|(key, _)| key.to_string())
        .collect()
}

/// Where a relocated lake's data files actually are, or `None` if the path
/// recorded in the lake is still valid.
fn relocated_data_path(conn: &Connection, lake: &Path) -> Option<String> {
    let recorded: String = conn
        .query_row(
            &format!(
                "SELECT value FROM ducklake_options({}) WHERE option_name = 'data_path'",
                sql_str(ALIAS)
            ),
            [],
            |row| row.get(0),
        )
        .ok()?;
    if Path::new(recorded.trim_end_matches('/')).is_dir() {
        return None;
    }

    let dir = lake.parent().unwrap_or_else(|| Path::new("."));
    // Prefer a directory beside the catalog with the same name as the
    // recorded one (usually `parquet/`), then the catalog's own directory.
    let named = Path::new(recorded.trim_end_matches('/'))
        .file_name()
        .map(|n| dir.join(n));
    let root = match named {
        Some(candidate) if candidate.is_dir() => candidate,
        _ => dir.to_path_buf(),
    };

    let mut path = root.to_string_lossy().to_string();
    if !path.ends_with('/') {
        path.push('/');
    }
    Some(path)
}

fn attach_error(path: &Path, e: duckdb::Error) -> anyhow::Error {
    if e.to_string().contains("Conflicting lock") {
        anyhow::anyhow!(
            "{} is locked by another process that has it open for writing",
            path.display()
        )
    } else {
        anyhow::Error::new(e).context(format!("attaching DuckLake catalog {}", path.display()))
    }
}

/// Quote an identifier for SQL: `a"b` → `"a""b"`.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a string literal for SQL: `it's` → `'it''s'`.
fn sql_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
    fn quoting_escapes_embedded_delimiters() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(sql_str("it's"), "'it''s'");
    }

    #[test]
    fn hive_keys_reads_partition_columns() {
        assert_eq!(
            hive_keys("/lake/obs/GEO_LEVEL=Country/f.parquet"),
            vec!["GEO_LEVEL".to_string()]
        );
        assert!(hive_keys("/lake/geo/f.parquet").is_empty());
    }

    #[test]
    fn partition_filter_is_quoted() {
        let table = TableInfo {
            schema: "s".into(),
            name: "t".into(),
            rows: 0,
            file_count: 0,
            file_size: 0,
            delete_file_count: 0,
            partition_cols: vec!["k".into()],
        };
        let partition = Partition {
            values: vec![("k".into(), "it's".into())],
            rows: 1,
        };
        let source = LakeSource {
            table: table.sql_ref(),
            filter: Some(format!(
                "{} = {}",
                quote_ident(&partition.values[0].0),
                sql_str(&partition.values[0].1)
            )),
        };
        assert!(source.relation().ends_with("WHERE \"k\" = 'it''s'"));
    }

    #[test]
    fn ordered_appends_sort_keys() {
        let source = LakeSource {
            table: "\"t\"".into(),
            filter: None,
        };
        let sql = source.ordered(&[("a".into(), true), ("b".into(), false)]);
        assert!(sql.ends_with("ORDER BY \"a\" ASC, \"b\" DESC"));
    }

    #[test]
    fn human_helpers_format() {
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_count(1_140_560_738), "1,140,560,738");
    }
}
