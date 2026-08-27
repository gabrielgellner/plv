# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`plv` is a terminal UI viewer for CSV and Parquet files, built with Rust. It uses [Polars](https://pola.rs/) for lazy data loading (larger-than-memory files) and [ratatui](https://ratatui.rs/) + crossterm for the TUI. The goal is a csvlens-like viewer with vim navigation.

## Commands

```bash
cargo build
cargo build --release      # optimized: LTO + strip
cargo run -- path/to/file.csv
cargo run -- path/to/file.parquet
cargo run -- path/to/bundle/          # DuckLake bundle (dir containing *.ducklake)
cargo run --example lakedump -- path/to/bundle/   # dump what the catalog reader sees
cargo test
cargo test <test_name>     # run a single test
```

## Architecture (3 layers)

```
src/
  main.rs         CLI (clap), terminal init — thin wrapper over the `plv` lib
  lib.rs          library root (bin and examples both use it)
  app.rs          App layer: event loop, state, key bindings, two screens
  lake.rs         DuckLake browse state: table list ↔ file pane ↔ open scope
  data/
    loader.rs     detect format by extension, return LazyFrame
    store.rs      scroll state + lazy data fetching
    catalog.rs    DuckLake catalog reader (DuckDB metadata → Polars scans)
  ui/
    table.rs      DataTable widget: renders DataFrame as a table
    browser.rs    Browser widget + cursor/scroll state for catalog lists
    help.rs       Help widget: the `?` key-binding overlay
    statusbar.rs  StatusBar widget: file/row/col position + help
```

**Data layer (`src/data/`)**
- `loader.rs`: detects `.csv`/`.parquet` by extension and opens a `LazyFrame`. Paths are converted to `PlRefPath` for the polars 0.53 API.
- `store.rs`: `Store` owns the `LazyFrame` and tracks `row_offset`/`viewport_rows`. Every scroll calls `lf.clone().slice(offset, height).collect()` — only the visible rows are ever materialized. Also exposes `schema: SchemaRef` for column metadata.

**UI layer (`src/ui/`)**
- `DataTable`: computes per-column display widths from the current view, determines which columns fit given the terminal width (starting from `col_offset`), then renders a ratatui `Table` with a row-number column on the left. Alternating row background.
- `StatusBar`: single-line bar showing filename, row range, column position, and key help.

**App layer (`src/app.rs`)**
- `App` owns `Option<Store>`, `col_offset`, and an optional `error: String`.
- `run()`: loads the file into a `Store` sized to the terminal, then enters the event loop.
- `draw()`: updates `store.viewport_rows` on resize, then renders `DataTable` + `StatusBar` (or an error/usage message if no file is loaded).
- Vim key bindings: `j/k` (±1 row), `Ctrl+d/u` (half page), `g/G` (top/bottom), `h/l` (±1 column), `H` (leftmost column), `q` (quit). Arrow keys mirror `j/k/h/l`.

## Polars 0.53 API notes

The `lazy` and `parquet` features must be explicitly enabled (they are not in the default feature set):
```toml
polars = { version = "0.53.0", features = ["lazy", "parquet"] }
```

Key API differences from older polars:
- `LazyFrame::schema()` → `LazyFrame::collect_schema()` (takes `&mut self`)
- `DataFrame::get_columns()` → `DataFrame::columns()` (returns `&[Column]`)
- `count()` expression → `len()` expression
- `LazyCsvReader::new(path)` and `LazyFrame::scan_parquet(path, args)` both take `PlRefPath`, converted via `PlRefPath::try_from_path(&Path)`

## DuckLake support

Pointing plv at a `.ducklake` file — or a directory containing one — opens the
**catalog browser** instead of the table viewer.

**Screens.** `App` has two: `Screen::Browser` (the catalog) and `Screen::Viewer`
(the existing `DataTable`). `lake.rs` holds the browse state: `Level::Tables`
lists the lake's tables, `Level::Files { table }` lists that table's Parquet
files, and `Scope` records what the viewer is currently scanning.

Browser keys: `j/k` move, `g/G` top/bottom, `l`/`f` descend into the file pane,
`T` list snapshots, `h`/`Esc` back, `Enter` open the selection, `a` open the
whole table from within the file pane. In the viewer, `f` returns to the file
pane, `b` to the browser, and `T` to the snapshot picker.

`?` opens a key-binding overlay whose contents follow the current screen
(`App::help_sections`); any key dismisses it. The status bar only has room for a
few hints, so it degrades to `?:help` and then to nothing rather than truncating
the position readout — the overlay is the authoritative in-app reference.

**Time travel.** `Enter` on a snapshot re-runs `Catalog::open_at` for that id and
replaces the whole `Lake`. If the table the viewer was showing still exists at
the target snapshot it is reopened by *name*, so the same data can be compared
across snapshots; a file-level scope widens to the whole table, because file ids
are not stable across snapshots.

**`data/catalog.rs`.** DuckDB is used *only* to read catalog metadata; Polars
remains the query engine. `Catalog::open` reads snapshots, schemas, tables,
columns, partition keys and data files, then `scan_files` builds a `LazyFrame`.

Things the reader has to get right, all of which the census bundle exercises:

- **Read-only.** The connection uses `AccessMode::ReadOnly`. A read-write handle
  takes an exclusive lock, which would both risk mutating a lake and lock out
  other readers. A lock held by a writer is reported as a plain "locked by
  another process" message.
- **Snapshot filtering.** Every catalog row carries `begin_snapshot`/
  `end_snapshot`; `visible(alias)` builds the predicate that pins a query to one
  snapshot. Qualify it with the table alias — the columns are ambiguous in joins.
- **Path resolution.** `ducklake_metadata.data_path` is an absolute path written
  by the machine that built the lake, so it is wrong once a bundle moves.
  `resolve_data_root` prefers it only if it still exists, then falls back to the
  same-named directory beside the catalog file.
- **Partition columns.** Hive partition values live in the directory path, not
  in the Parquet file. Rather than rely on path inference, each file is scanned
  separately and its partition values are attached as literal columns taken from
  the catalog, then projected into catalog column order.
- **Inlined rows.** DuckDB can hold recent rows in the catalog database instead
  of Parquet (`ducklake_inlined_data_*`). A Parquet-only scan silently misses
  them, so `TableInfo::inlined_rows` is surfaced in the browser and as a warning
  when the table is opened. They are *not* currently merged into the view.
- **Snapshot-scoped stats.** `TableInfo::record_count`/`file_size` are summed
  from the files visible at the resolved snapshot, *not* read from
  `ducklake_table_stats` — that table is a running total for the current state
  and would report today's row count while time travelling to a snapshot taken
  before the data landed.
- **Row counts.** Use `Store::with_row_count` with the catalog's per-file
  `record_count`. Counting by scanning cost ~7s on the 1.1B-row census table for
  a number the catalog already stores exactly.
