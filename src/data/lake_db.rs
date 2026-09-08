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
                    format!(
                        "{} {}",
                        quote_ident(name),
                        if *asc { "ASC" } else { "DESC" }
                    )
                })
                .collect();
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        sql
    }
}

/// What every page of one lake table needs to know beyond the query itself:
/// the shape it must come back in, and where its rows physically are.
///
/// Built once when the table is opened.
#[derive(Clone, Debug, Default)]
pub struct Reader {
    /// The table's columns and the types plv shows them as, asked of DuckDB
    /// once rather than guessed per page.
    ///
    /// A page used to be typed by the first non-null value *in that page*, so
    /// a column that is empty here and a number three screens down changed
    /// type as you scrolled. The delimited path has said for a while that a
    /// chunk must be given the schema rather than infer its own; this is the
    /// same rule, arriving late.
    pub columns: Vec<(String, DataType)>,
    /// Where the rows are, when the catalog will say plainly enough to be
    /// trusted. Absent means every page goes through `LIMIT`/`OFFSET`.
    pub files: Option<FileMap>,
}

impl Reader {
    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Put a page into the shape the table declares, whichever way it was
    /// read. A column DuckDB handed back as text because this page happened
    /// to be empty becomes the number it is.
    fn shape(&self, df: DataFrame) -> Result<DataFrame> {
        if self.columns.is_empty() {
            return Ok(df);
        }
        let cast: Vec<Column> = df
            .columns()
            .iter()
            .map(|column| {
                match self
                    .columns
                    .iter()
                    .find(|(name, _)| name.as_str() == column.name().as_str())
                {
                    Some((_, dtype)) => column.cast(dtype).unwrap_or_else(|_| column.clone()),
                    None => column.clone(),
                }
            })
            .collect();
        Ok(DataFrame::new(df.height(), cast)?)
    }
}

/// Where a table's rows physically live, so a page can be seeked to rather
/// than counted to.
///
/// `LIMIT n OFFSET m` makes DuckDB produce and discard every row up to the
/// offset: measured against a 1.14-billion-row table, the last page costs 2.7
/// seconds against 137ms for the first. The rows are in parquet files, though,
/// and a parquet reader can seek by row group — the same data as a file reads
/// anywhere in 26ms. All that is missing is which file a row number lands in,
/// and the catalog knows: it records how many rows each file holds.
///
/// So this is the catalog's own arithmetic, kept to hand. It is **entirely
/// optional**: every way of failing to build it, and every doubt while using
/// it, returns `None` and leaves the query to go the way it always did. That
/// is what makes reading the files directly safe here, where
/// re-implementing the format's reader was not — the reader is still
/// DuckDB's, and this only ever answers *where*.
#[derive(Clone, Debug)]
pub struct FileMap {
    files: Vec<FileSpan>,
    /// The columns the logical table has, in its order. A file that does not
    /// have them all is a file this cannot read.
    columns: Vec<String>,
}

#[derive(Clone, Debug)]
struct FileSpan {
    path: PathBuf,
    /// Row number of this file's first row, in the table.
    start: usize,
    rows: usize,
}

impl FileMap {
    /// One page, read straight out of the files it falls in — or `None` where
    /// anything at all is not as this expects, which leaves the caller to ask
    /// DuckDB the slow way.
    pub fn page(&self, offset: usize, limit: usize) -> Option<DataFrame> {
        let mut page: Option<DataFrame> = None;
        let mut taken = 0usize;
        for file in &self.files {
            if taken >= limit {
                break;
            }
            let wanted = offset + taken;
            if wanted < file.start || wanted >= file.start + file.rows {
                continue;
            }
            let local = wanted - file.start;
            let take = (file.rows - local).min(limit - taken);
            let part = self.read(file, local, take)?;
            taken += part.height();
            page = Some(match page {
                None => part,
                Some(mut so_far) => {
                    so_far.vstack_mut(&part).ok()?;
                    so_far
                }
            });
        }
        page.or_else(|| (offset >= self.rows()).then(DataFrame::empty))
    }

    /// Rows `offset..offset + limit` of one file.
    ///
    /// A file that gives back fewer rows than the catalog said it holds is a
    /// file whose count disagrees with the catalog, and nothing after it can
    /// be trusted to be where this map says it is — so that is a `None` too.
    fn read(&self, file: &FileSpan, offset: usize, limit: usize) -> Option<DataFrame> {
        let path = PlRefPath::try_from_path(&file.path).ok()?;
        let frame = LazyFrame::scan_parquet(path, Default::default())
            .ok()?
            .slice(offset as i64, limit as u32)
            // By name and in the table's order: a file written before a column
            // was added, or with them in another order, must not be read as
            // though its columns were the ones plv is showing.
            .select(
                self.columns
                    .iter()
                    .map(|name| col(name.as_str()))
                    .collect::<Vec<_>>(),
            )
            .collect()
            .ok()?;
        (frame.height() == limit).then_some(frame)
    }

    fn rows(&self) -> usize {
        self.files.last().map_or(0, |f| f.start + f.rows)
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
    /// The attachment itself, for a caller that wants to ask its own
    /// question of the lake.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

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
            &format!(
                "SELECT id FROM ducklake_current_snapshot({})",
                sql_str(ALIAS)
            ),
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
                .query_row(
                    &format!("SELECT count(*) FROM {}", table.sql_ref()),
                    [],
                    |r| r.get::<_, i64>(0),
                )
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
        let keys: Vec<String> = table
            .partition_cols
            .iter()
            .map(|c| quote_ident(c))
            .collect();
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

    /// Where this table's rows are, if the catalog will say so plainly.
    ///
    /// `None` at the first sign of anything this cannot account for, and the
    /// caller then pages the way it always did. The last check is the one that
    /// does most of the work: the file counts must add up to what `count(*)`
    /// says. A delete file that removed rows makes the sum too high, rows
    /// inlined in the catalog make it too low, and a snapshot this read the
    /// wrong file set for makes it one or the other — so the three of them are
    /// caught by arithmetic rather than by a growing list of conditions.
    ///
    /// Only for the whole table: a partition is a `WHERE` clause, and which
    /// files satisfy it is a question about physical layout that this
    /// deliberately does not ask.
    pub fn file_map(
        &self,
        table: &TableInfo,
        source: &LakeSource,
        columns: &[String],
    ) -> Option<FileMap> {
        if source.filter.is_some() {
            return None;
        }
        let snapshot = self.snapshot.or_else(|| self.current_snapshot().ok())?;
        let table_id = self.table_id(table)?;

        // Paths as the extension resolves them, which is the one place that
        // knows how a relative path in the catalog becomes a file on disk.
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT data_file, delete_file FROM ducklake_list_files({}, {}, schema => {})",
                sql_str(ALIAS),
                sql_str(&table.name),
                sql_str(&table.schema)
            ))
            .ok()?;
        let resolved: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .ok()?
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if resolved.iter().any(|(_, delete)| delete.is_some()) {
            return None;
        }

        // Counts and order, from the catalog the extension keeps beside the
        // lake. An internal name, so a version that renames it simply loses
        // the fast path rather than breaking.
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT path, record_count, mapping_id
                 FROM __ducklake_metadata_{ALIAS}.ducklake_data_file
                 WHERE table_id = ?1
                   AND begin_snapshot <= ?2
                   AND (end_snapshot IS NULL OR end_snapshot > ?2)
                 ORDER BY row_id_start, data_file_id"
            ))
            .ok()?;
        let listed: Vec<(String, i64, Option<i64>)> = stmt
            .query_map(params![table_id, snapshot], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .ok()?
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if listed.is_empty() || listed.iter().any(|(_, _, mapping)| mapping.is_some()) {
            // A mapping means the file's columns are not the table's, which is
            // exactly the thing the extension exists to handle.
            return None;
        }

        let mut files = Vec::with_capacity(listed.len());
        let mut start = 0usize;
        for (path, count, _) in listed {
            // The catalog's path is relative to a prefix this does not try to
            // reconstruct; the resolved list has the same file spelled whole.
            let mut matches = resolved
                .iter()
                .filter(|(full, _)| full.ends_with(&path) || *full == path);
            let (full, _) = matches.next()?;
            if matches.next().is_some() {
                return None; // ambiguous, so not worth guessing
            }
            let rows = usize::try_from(count).ok()?;
            files.push(FileSpan {
                path: PathBuf::from(full),
                start,
                rows,
            });
            start += rows;
        }

        (start == self.count(source).ok()?).then_some(FileMap {
            files,
            columns: columns.to_vec(),
        })
    }

    /// The table's columns, in its order — what a file read has to produce.
    pub fn column_names(&self, source: &LakeSource) -> Result<Vec<String>> {
        column_names(&self.conn, source)
    }

    fn table_id(&self, table: &TableInfo) -> Option<i64> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT t.table_id
                 FROM __ducklake_metadata_{ALIAS}.ducklake_table t
                 JOIN __ducklake_metadata_{ALIAS}.ducklake_schema s
                   ON s.schema_id = t.schema_id
                 WHERE t.table_name = ?1 AND s.schema_name = ?2
                   AND t.end_snapshot IS NULL"
            ))
            .ok()?;
        let ids: Vec<i64> = stmt
            .query_map(params![table.name, table.schema], |row| row.get(0))
            .ok()?
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        match ids.as_slice() {
            [only] => Some(*only),
            _ => None,
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
        page_with(
            &self.conn,
            source,
            &self.reader(None, source),
            sort,
            offset,
            limit,
        )
    }

    /// The shape and layout of one table, asked once so every page agrees.
    pub fn reader(&self, table: Option<&TableInfo>, source: &LakeSource) -> Reader {
        let columns = self.column_types(source).unwrap_or_default();
        let files = table.and_then(|table| {
            let names: Vec<String> = columns.iter().map(|(name, _)| name.clone()).collect();
            self.file_map(table, source, &names)
        });
        Reader { columns, files }
    }

    /// The table's columns and the types plv will show them as.
    ///
    /// `DESCRIBE` answers from the catalog rather than by reading anything,
    /// and the mapping is the one the viewer has always used: integers,
    /// floats and booleans keep their type and everything else is text.
    fn column_types(&self, source: &LakeSource) -> Result<Vec<(String, DataType)>> {
        let mut stmt = self
            .conn
            .prepare(&format!("DESCRIBE SELECT * FROM {}", source.relation()))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(name, sql_type)| (name, shown_as(&sql_type)))
            .collect())
    }
}

/// Fetch one page as a `DataFrame`. Free function so a background thread can
/// call it with its own cloned connection.
pub fn page_with(
    conn: &Connection,
    source: &LakeSource,
    reader: &Reader,
    sort: &[(String, bool)],
    offset: usize,
    limit: usize,
) -> Result<DataFrame> {
    // Straight to the rows when nothing has been asked that changes which
    // rows they are. A sort puts them in an order no file holds, and a
    // partition filter is a question about which files answer it — both go
    // to DuckDB, which is what it is for.
    if sort.is_empty()
        && source.filter.is_none()
        && let Some(page) = reader
            .files
            .as_ref()
            .and_then(|map| map.page(offset, limit))
    {
        return reader.shape(page);
    }
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
    reader.shape(DataFrame::new(height, series)?)
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
/// How plv shows a DuckDB type: numbers and booleans as themselves, and
/// everything else — dates, timestamps, structs, lists — as text, which is
/// what the viewer draws anyway.
fn shown_as(sql_type: &str) -> DataType {
    let head = sql_type.split('(').next().unwrap_or(sql_type).trim();
    match head {
        "BOOLEAN" => DataType::Boolean,
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT" | "USMALLINT"
        | "UINTEGER" | "UBIGINT" | "UHUGEINT" => DataType::Int64,
        "FLOAT" | "DOUBLE" | "REAL" | "DECIMAL" | "NUMERIC" => DataType::Float64,
        _ => DataType::String,
    }
}

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

    /// Three files of known length, written as parquet, so the arithmetic
    /// that turns a row number into a file and an offset is exercised against
    /// a real reader rather than a mock of one.
    fn three_files(name: &str) -> FileMap {
        let dir = std::env::temp_dir().join("plv-lake-tests").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = Vec::new();
        let mut start = 0usize;
        for (i, rows) in [5usize, 3, 4].into_iter().enumerate() {
            let path = dir.join(format!("part{i}.parquet"));
            let n: Vec<i64> = (start..start + rows).map(|v| v as i64).collect();
            let tag: Vec<String> = n.iter().map(|v| format!("row{v}")).collect();
            let mut df = df! { "n" => n, "tag" => tag }.unwrap();
            let mut out = std::fs::File::create(&path).unwrap();
            ParquetWriter::new(&mut out).finish(&mut df).unwrap();
            files.push(FileSpan { path, start, rows });
            start += rows;
        }
        FileMap {
            files,
            columns: vec!["n".to_string(), "tag".to_string()],
        }
    }

    fn ns(df: &DataFrame) -> Vec<i64> {
        df.column("n")
            .unwrap()
            .i64()
            .unwrap()
            .into_no_null_iter()
            .collect()
    }

    #[test]
    fn a_page_is_read_from_the_file_its_rows_fall_in() {
        let map = three_files("pages");
        assert_eq!(ns(&map.page(0, 3).unwrap()), [0, 1, 2]);
        assert_eq!(ns(&map.page(5, 3).unwrap()), [5, 6, 7], "the second file");
        assert_eq!(ns(&map.page(8, 4).unwrap()), [8, 9, 10, 11], "the third");
    }

    #[test]
    fn a_page_that_straddles_files_is_stitched_from_both() {
        let map = three_files("straddle");
        assert_eq!(ns(&map.page(3, 4).unwrap()), [3, 4, 5, 6]);
        assert_eq!(
            ns(&map.page(2, 10).unwrap()),
            [2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            "across all three"
        );
    }

    #[test]
    fn a_page_past_the_end_is_empty_rather_than_wrong() {
        let map = three_files("past-end");
        assert_eq!(map.page(12, 5).unwrap().height(), 0);
        // A page that runs off the end gives what there is.
        assert_eq!(ns(&map.page(10, 5).unwrap()), [10, 11]);
    }

    /// A file whose columns are not the table's is one this cannot read, and
    /// saying so is what sends the caller back to DuckDB.
    #[test]
    fn a_file_without_the_tables_columns_is_refused() {
        let mut map = three_files("columns");
        map.columns.push("missing".to_string());
        assert!(map.page(0, 3).is_none());
    }

    #[test]
    fn a_type_is_what_the_table_says_it_is_not_what_a_page_happens_to_hold() {
        assert_eq!(shown_as("BIGINT"), DataType::Int64);
        assert_eq!(shown_as("DECIMAL(18,3)"), DataType::Float64);
        assert_eq!(shown_as("BOOLEAN"), DataType::Boolean);
        // Everything the viewer draws as text, whatever DuckDB calls it.
        assert_eq!(shown_as("TIMESTAMP"), DataType::String);
        assert_eq!(shown_as("STRUCT(a INTEGER)"), DataType::String);
    }

    /// The shape is applied whichever way the page was read, so a column that
    /// is empty on this page is still the number the table says it is.
    #[test]
    fn a_page_is_cast_to_the_shape_the_table_declares() {
        let reader = Reader {
            columns: vec![
                ("n".to_string(), DataType::Int64),
                ("empty".to_string(), DataType::Float64),
            ],
            files: None,
        };
        let df = df! {
            "n" => [1i64, 2],
            // What a page of nothing but nulls comes back as.
            "empty" => [None::<String>, None],
        }
        .unwrap();
        let shaped = reader.shape(df).unwrap();
        assert_eq!(
            shaped.column("empty").unwrap().dtype(),
            &DataType::Float64,
            "so the column does not change type as you scroll"
        );
    }

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
