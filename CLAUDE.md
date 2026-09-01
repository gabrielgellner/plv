# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`plv` is a terminal UI viewer and editor for CSV, TSV and Parquet files, built with Rust. It uses [Polars](https://pola.rs/) for lazy data loading (larger-than-memory files) and [ratatui](https://ratatui.rs/) + crossterm for the TUI. The goal is a csvlens-like viewer with vim navigation.

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
    store.rs      scroll state + data fetching (Polars or lake-backed)
    edit.rs       the edit buffer: a sparse overlay + undo history
    writer.rs     splice edits back into the file, byte-preserving
    lake_db.rs    DuckLake access via DuckDB's ducklake extension
  view.rs         the view language: parse and check :select/:filter/:sort
  ui/
    table.rs      DataTable widget: renders DataFrame as a table
    browser.rs    Browser widget + cursor/scroll state for catalog lists
    help.rs       Help widget: the `?` key-binding overlay
    statusbar.rs  StatusBar widget: file/row/col position + help
```

**Data layer (`src/data/`)**
- `loader.rs`: detects `.csv`, `.tsv`/`.tab`, `.txt` and `.parquet` by extension and opens a `LazyFrame`. Delimited text goes through `LazyCsvReader` with an explicit `with_separator`. `.txt` names no delimiter, so `sniff_delimiter()` picks one: it counts tab/comma/semicolon/pipe outside quoted spans on the first few lines and takes the candidate that occurs the same non-zero number of times on every line, falling back to a tab. Paths are converted to `PlRefPath` for the polars 0.53 API.
- `store.rs`: `Store` owns the `LazyFrame` and tracks `row_offset`/`viewport_rows`. Every scroll calls `lf.clone().slice(offset, height).collect()` — only the visible rows are ever materialized. Also exposes `schema: SchemaRef` for column metadata, and owns the edit `Overlay` (see below).

**UI layer (`src/ui/`)**
- `DataTable`: computes per-column display widths from the current view, determines which columns fit given the terminal width (starting from `col_offset`), then renders a ratatui `Table` with a row-number column on the left. Alternating row background.
- The row-number gutter counts from the cursor by default, nvim's hybrid `number` + `relativenumber`: each row shows its distance and the cursor row shows its own number, so `{n}j` and `{n}G` can be read off rather than worked out. The current line is left-aligned where the distances are right-aligned, which is what makes it read as outdented — and both fill the same width, so the column does not shift as the cursor moves. `#` switches to plain absolute numbering. `row_num_width()` still sizes the column from `row_offset` and only grows at powers of ten.
- Cells are rendered with `AnyValue::str_value()` rather than its `Display`, which quotes strings for debugging. An empty or absent field shows a recessive · rather than the word `null`: the two are the same thing to plv — both are written back as an empty field — and `null` as text would collide with a field whose value really is "null".
- `StatusBar`: single-line bar showing filename, row range, column position, and key help.

**App layer (`src/app.rs`)**
- `App` owns `Option<Store>`, `col_offset`, and an optional `error: String`.
- `run()`: loads the file into a `Store` sized to the terminal, then enters the event loop.
- `draw()`: updates `store.viewport_rows` on resize, then renders `DataTable` + `StatusBar` (or an error/usage message if no file is loaded).
- Vim key bindings: `j/k` (±1 row), `Ctrl+d`/`Ctrl+u` (half a screen: the view and the cursor move together, as in vim, rather than the cursor walking to the edge first), `gg/G` (top/bottom), `{n}gg`/`{n}G` (row n), `zz`/`zt`/`zb` (scroll the cursor row to the middle/top/bottom of the viewport), `h/l` and `{n}h`/`{n}l` (columns, counted like `j`/`k`), `0`/`$` (scroll so the first/last column sits at its edge), `H` (leftmost column), `q` (quit). Arrow keys mirror `j/k/h/l`.
- Horizontal movement stops where the last column reaches the right edge, in every selection mode: scrolling past it would pad the view with empty space instead of data. `ui::col_offset_showing()` answers that question, and lives beside the renderer that has to agree with it — the app layer used to keep its own copy of the arithmetic and the two drifted apart.

## Editing

Delimited text — `.csv`, `.tsv`, `.tab`, `.txt` — can be edited in place.
Parquet and lake tables stay read-only: Parquet is genuinely typed, so a
one-cell change means rewriting the whole file against a schema.

**Edits live in a buffer, not in a frame.** A `LazyFrame` cannot be mutated, and
collecting the file to edit it would throw away the larger-than-memory property
exactly when it matters. So `data/edit.rs` holds an `Overlay`: a sparse
`BTreeMap<row, BTreeMap<col, String>>` keyed by position in the *source file*,
grouped by row because that is the shape both readers want — the renderer asks
for a page, the writer walks records in order. Memory follows the number of
edits, not the size of the file. `Store::fetch` stamps the overlay onto every
page it returns, so no scroll path can forget it.

Undo history is a stack of **transactions**, not single cells, so one fill over
a visual selection is one `u`. `Overlay::clear()` drops the history with the
edits: undoing past a write would resurrect changes the user believes they saved.

**Values are text.** The file on disk is untyped; the types plv shows are
Polars' inference over it. A column takes an edit by going through text and
comes back typed if it can — typing `42` into a number is still a number — and
only a value that genuinely does not fit leaves the column as text, with a
warning. Note `strict_cast` and not `cast`: a plain cast turns an unparseable
value into a *null*, silently swallowing the edit.

**Writing splices bytes** (`data/writer.rs`). Re-serializing the frame would
reformat every line — float formatting, quoting style, nulls vs empty strings —
turning a one-cell change into a whole-file diff. Instead a byte-at-a-time
RFC 4180 scanner streams the original through and substitutes only the edited
fields, so quoting, line endings, a BOM, a missing final newline and every
untouched line survive exactly. It is O(1) in memory, for the same reason the
read path is lazy.

Two guards run before the rename: the record count must match the view's row
count, and every edit must have found a field to land in (which catches ragged
rows). The output is fsynced beside the target and moved into place, so a failed
write leaves the original intact. An open-time `Stamp` (length + mtime) refuses a
write to a file that changed underneath the buffer; `:w!` forces.

**Polars does not skip blank lines** — it reads an empty line as a row of nulls,
trailing ones included — so every line ending closes a record in the writer too.
Getting this wrong puts edits one line off; `record_numbering_agrees_with_the_polars_reader`
pins it against the real reader rather than against the assumption.

**Editing is refused on a sorted view**, because a sorted page's rows are not the
file's rows and an edit could not be told which line it belongs to.
`Store::edit_blocked()` returns the reason as a string, so the rule and its
explanation cannot drift apart.

**Keys.** `i`/`a`/`c` open a cell (caret at the front, at the end, or empty), `x`
clears it, `u`/`Ctrl+r` undo and redo, `y`/`p` yank and paste through an internal
register (no system clipboard). `v` starts a visual selection whose shape follows
the `Tab` mode — whole rows, whole columns, or a rectangle. Over a selection `c`
replaces every cell while `i` and `a` prepend and append to what is already
there, following vim's blockwise `I` and `A`; those two read the block first, so
they see rows below the viewport. Block operators are capped at `MAX_BLOCK` cells,
because a column-mode selection covers every row in the file.

`:` opens an ex line: `:w`, `:w!`, `:w path`, `:q`, `:q!`, `:wq`, `:x`. Bare `q`
and `:q` refuse while edits are unwritten. The status bar carries a `[+n]` count
and edited cells render in red.

## The view language (`src/view.rs`)

`:select`, `:hide`, `:filter` and `:sort` shape what the viewer shows. The
module parses and checks; nothing in it touches a `LazyFrame`.

**Validation happens when the line is typed, not when the frame collects.**
Polars is lazy, so `filter count > abc` does not fail where it was written — it
fails inside a later `collect`, as a query-planner error naming nodes the user
never typed. The schema is in hand at the prompt, so column names resolve to
indices there (which also settles duplicate names) and literals are checked
against the column's dtype there. Errors carry the byte span of the word that
caused them, so the prompt can underline it.

**The view is state, not a pipeline.** Each command replaces its own slot, so
`:select a b` then `:select c` shows `c` rather than trying to select `c` from a
frame already narrowed to `a b`. `:hide` writes to the same slot `:select` does.
`Store::sort` already worked this way.

Evaluation order is fixed independently of the order commands were typed:
**filter → sort → select**, as in SQL, so a filter or sort can name a column
that is not on show.

Grammar, deliberately closed: `~` and `!~` are regex and read any column as text
(as `/` search does); other comparisons require a literal matching the column's
type; an empty literal `""` means the cells with nothing in them, matching how
plv renders and writes empty fields elsewhere. Conditions join with `and` only —
no `or` and no parentheses, because precedence cannot be introduced later
without changing what already-written commands mean.

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

**Lake data is read through DuckDB's `ducklake` extension**, not by plv. The
extension is DuckLake's reference reader and presents the *logical* table:
inlined rows merged, delete files applied, schema evolution and column mapping
handled. Reading the Parquet files directly would mean re-implementing all of
that against a 28-table spec — plv did that once, and it leaked (inlined rows
went missing, delete files were ignored). CSV and Parquet still go through
Polars; `Store` carries a `Source` enum with one arm for each.

**Screens.** `App` has two: `Screen::Browser` (the lake) and `Screen::Viewer`
(the existing `DataTable`). `lake.rs` holds the browse state: `Level::Tables`
lists the lake's tables, `Level::Partitions { table }` lists that table's
partition values, `Level::Snapshots` lists snapshots, and `Scope` records what
the viewer is scanning.

Browser keys: `j/k` move, `g/G` top/bottom, `l`/`f` descend into the partition
list, `T` list snapshots, `h`/`Esc` back, `Enter` open the selection, `a` open
the whole table from within the partition list. In the viewer, `f` returns to
the partitions, `b` to the browser, `T` to the snapshot picker.

`?` opens a key-binding overlay whose contents follow the current screen
(`App::help_sections`); any key dismisses it. The status bar only has room for a
few hints, so it degrades to `?:help` and then to nothing rather than truncating
the position readout — the overlay is the authoritative in-app reference.

**Time travel** is an ATTACH option (`SNAPSHOT_VERSION`), so selecting a
snapshot re-opens the lake rather than rewriting every query. If the table the
viewer was showing still exists at the target snapshot it is reopened by *name*,
so the same data can be compared across snapshots.

### `data/lake_db.rs`

- **Read-only.** Attached with `READ_ONLY`. A lock held by a writer is reported
  as a plain "locked by another process" message.
- **Relocation.** DuckLake rejects a `DATA_PATH` that disagrees with the one
  recorded in the lake, so plv attaches plainly first, checks the recorded path
  via `ducklake_options()`, and only re-attaches with `DATA_PATH` +
  `OVERRIDE_DATA_PATH` when that path is gone.
- **Partition columns** are read off the Hive-style data file paths from
  `ducklake_list_files()`. The logical reader deliberately exposes no partition
  metadata; the paths only tell us *which* columns to group by, and the values
  shown come from a real `GROUP BY`.
- **Row counts** come from `count(*)`, which the extension answers from catalog
  statistics — 0.01s on the 1.1B-row census table.
- **Partition lists** cost a `GROUP BY` (~0.5s on that table), so they load on
  demand rather than up front.
- **Background work** clones the connection with `try_clone()`, which shares the
  attached lake instead of paying the ~20ms ATTACH again.
- **Type mapping.** Page results are converted to Polars columns; integers,
  floats and booleans keep their type, everything else renders as text.
