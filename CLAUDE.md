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
cargo test
cargo test <test_name>     # run a single test
```

## Architecture (3 layers)

```
src/
  main.rs         CLI (clap), terminal init
  app.rs          App layer: event loop, state, key bindings
  data/
    loader.rs     detect format by extension, return LazyFrame
    store.rs      scroll state + lazy data fetching
  ui/
    table.rs      DataTable widget: renders DataFrame as a table
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
