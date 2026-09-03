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
  picker.rs       the column picker: what is on show and what is pinned
  data/
    loader.rs     detect format by extension, return LazyFrame
    store.rs      scroll state + data fetching (Polars or lake-backed)
    edit.rs       the edit buffer: a sparse overlay + undo history
    writer.rs     splice edits back into the file, byte-preserving
    lake_db.rs    DuckLake access via DuckDB's ducklake extension
    rows.rs       row sets: which rows a :filter matched, and paging by index
  view.rs         the view language: parse and check :select/:filter/:sort
  complete.rs     completion for the `:` line, per position
  ui/
    table.rs      DataTable widget: renders DataFrame as a table
    browser.rs    Browser widget + cursor/scroll state for catalog lists
    help.rs       Help widget: the `?` key-binding overlay
    panel.rs      Panel widget: the candidate list above the status bar
    statusbar.rs  StatusBar widget: file/row/col position + help
```

**Data layer (`src/data/`)**
- `index.rs`: a delimited file has to be read through once to know how many rows it has, so that pass records where the rows are too — the byte offset of every `STRIDE`th row. A page then seeks to the nearest checkpoint and parses forward a bounded number of rows instead of counting from the top. On a 30GB CSV that is 46s → 3ms for the last page, and the scan holds nothing: peak memory for opening, paging, editing and rewriting that file is 57MB. The scan is quote-aware, and a test holds it, `writer.rs` and Polars to the same answer about what a record is. A page parsed out of the middle is given the schema plv already inferred, or a chunk would type its columns by whatever happens to be in those rows and the types would change as you scroll.
- `loader.rs`: detects `.csv`, `.tsv`/`.tab`, `.txt` and `.parquet` by extension and opens a `LazyFrame`. Delimited text goes through `LazyCsvReader` with an explicit `with_separator`. `.txt` names no delimiter, so `sniff_delimiter()` picks one: it counts tab/comma/semicolon/pipe outside quoted spans on the first few lines and takes the candidate that occurs the same non-zero number of times on every line, falling back to a tab. Paths are converted to `PlRefPath` for the polars 0.53 API.
- `store.rs`: `Store` owns the `LazyFrame` and tracks `row_offset`/`viewport_rows`. Every scroll calls `lf.clone().slice(offset, height).collect()` — only the visible rows are ever materialized. Also exposes `schema: SchemaRef` for column metadata, and owns the edit `Overlay` (see below).

**UI layer (`src/ui/`)**
- `DataTable`: computes per-column display widths from the current view, determines which columns fit given the terminal width (starting from `col_offset`), then renders a ratatui `Table` with a row-number column on the left. Alternating row background.
- `K` shows the cursor cell in full in the panel above the status bar, and stays on while the cursor moves — a column of long values is read by walking down it. It shares the panel strip with `:` completion; the two cannot both be wanted, since one belongs to the command line and the other to the table. Capped at half the screen, with the last line saying how much was left out rather than stopping in silence.
- Column widths set by hand (`z>`/`z<`/`z_`/`z=`) live in `App::widths`, keyed by **source** column so they follow a column through `:select`. They are display state, not view state — how wide a column is drawn is a fact about this screen, not about which rows and columns are being asked for — so they are deliberately not in the `View`, and `u` does not take them back: `u` is the edit buffer's undo, and mixing display noise into the data history would make it mean two things. `z=` is the reset. A set width is exempt from the default cap, and pushes later columns off the right edge rather than squeezing them, which is what a spreadsheet does. `ui::drawn_width` answers what a column is currently drawn at so the app layer does not re-derive the layout arithmetic — it did once, and the two drifted.
- Columns are **pinned** to the left edge with `zp` (`z|` unpins the lot), so a key or a label stays in sight while `h`/`l` walk the columns it is being read against. `App::pinned` is keyed by source column for the same reason `widths` is, and is display state for the same reason: which column stays in sight while you move sideways is a fact about this screen. `App::display_pins` is the crossing point — `Store` counts source columns and the widget counts display positions — so a pin on a column `:select` hides is not forgotten, it simply has nowhere to be drawn. In the renderer the pinned columns lead `vis_cols` and are then *skipped* by the scrolling walk, or a column both pinned and scrolled to would be drawn twice; `ui::col_offset_showing` pays for the block out of its budget and steps over pins on the way left, since they are already drawn and already paid for. A second `│` closes the block: the gutter already ends in one, and without it a pinned column reads as sitting beside the column next to it, which is the one thing it is not. Past what `ui::pin_fits` allows plv **refuses the pin** rather than drawing a table with no scrolling region — a view that silently stops answering `h` and `l` is worse than being told why. Where the last scrolling column still does not fit it is squeezed into what is left rather than overflowing, because an overflow is paid for out of the pinned columns, undoing the one thing they were set to do. The second key of a `z` command is lowercased (a shift held for `z<` lands on the `z` too), so unpinning everything is `z|` and not `zP` — and a stray shift on `zp` toggles rather than clearing.
- The row-number gutter counts from the cursor by default, nvim's hybrid `number` + `relativenumber`: each row shows its distance and the cursor row shows its own number, so `{n}j` and `{n}G` can be read off rather than worked out. The current line is left-aligned where the distances are right-aligned, which is what makes it read as outdented — and both fill the same width, so the column does not shift as the cursor moves. `#` switches to plain absolute numbering. `row_num_width()` still sizes the column from `row_offset` and only grows at powers of ten.
- Cells are rendered with `AnyValue::str_value()` rather than its `Display`, which quotes strings for debugging. An empty or absent field shows a recessive · rather than the word `null`: the two are the same thing to plv — both are written back as an empty field — and `null` as text would collide with a field whose value really is "null".
- `StatusBar`: single-line bar showing filename, row range, column position, and key help.

**App layer (`src/app.rs`)**
- `App` owns `Option<Store>`, `col_offset`, and an optional `error: String`.
- `run()`: loads the file into a `Store` sized to the terminal, then enters the event loop.
- `draw()`: updates `store.viewport_rows` on resize, then renders `DataTable` + `StatusBar` (or an error/usage message if no file is loaded).
- Vim key bindings: `j/k` (±1 row), `Ctrl+d`/`Ctrl+u` (half a screen: the view and the cursor move together, as in vim, rather than the cursor walking to the edge first), `gg/G` (top/bottom), `{n}gg`/`{n}G` (row n), `zz`/`zt`/`zb` (scroll the cursor row to the middle/top/bottom of the viewport), `h/l` and `{n}h`/`{n}l` (columns, counted like `j`/`k`), `0`/`$` (scroll so the first/last column sits at its edge), `H` (leftmost column), `zp`/`z|` (pin the cursor column at the left edge / unpin every column), `-` (hide the cursor column), `C` (the column picker), `q` (quit). Arrow keys mirror `j/k/h/l`.
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

**Rows are added with ids past the end of the file.** `o` and `O` note a row in
the overlay before some source row, and its cells live under an id numbered past
`total_rows` — so `(row, column)` keys, the overlay stamping and `Store::edit`
all work on a new row unchanged, and the two can never be confused. A page
leaves a blank row where one was added and the overlay fills it in. On write the
new rows go in ahead of the row they precede, with a field per column and the
file's own line ending. Deleting an added row takes it back out of the buffer
rather than striking it — there is no record in the file to drop, and the write
would be refused.

**Rows are struck, not removed.** `dd` and a visual `d` note a source row in
the overlay's struck set; nothing leaves the file until `:w`, so `u` puts it
back and the buffer stays proportional to what was changed rather than to the
file. The row space skips them — `row_count` subtracts them, `source_row` steps
over them — so deleting a thousand rows from a ten-million-row file is about a
millisecond and paging past them costs what it did before. In the writer a
struck record goes nowhere, terminator included, or the file would grow blank
lines where rows used to be. Deleting is refused under a filter or a sort, which
put an explicit list of rows on screen that taking one out of the middle would
mean rebuilding.

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

`-` hides the cursor column, and is sugar for typing `:hide <name>` —
deliberately nothing more. It goes through `App::apply_view_command`, the same
path a typed line takes, so there is one answer to which columns show rather
than two that can disagree; a separate hidden-set beside `widths` and `pinned`
would be the drift this file keeps warning about, because *whether* a column is
on show is exactly what the `View` is for. `:hide` reads the current list and
filters it, so `-` twice narrows twice where a second `:select` would replace
the first, and hiding the last column is refused by `View::apply` rather than by
the key. The cursor is left where the column was, on whatever moved into that
position — `dd`'s rule, and it falls out of the clamp in `after_view_change`.
There is no key that un-hides one column, since naming it is the only way to say
which, so the message says `:reset select` at the moment the user might want it.

`C` opens the **column picker** (`src/picker.rs`), the other direction: a list of
every column with a tick for shown and one for pinned, `-` and `p` to toggle,
`Enter` to apply and `Esc` to leave. `-` and not Space, so the key that takes a
column off the view is the same one in the list as in the table — out there it
can only hide, since there is nothing on screen to un-hide, while in here the
state is in front of you and it toggles. `q` is deliberately **unbound**:
everywhere else in vim it closes a window, and a window is a view, so closing one
never destroys work — the picker holds changes that are not applied yet, so it
cannot close harmlessly and does not borrow the letter. `Esc` discards, the way
it discards a half-typed `:` line. It holds a **working copy**, so cancelling
costs nothing and `Enter` is the only thing that changes anything — and what it
produces is a `view::Command::Select` through `App::apply_view_command`, the same
path `:select` and `-` take, so it is a way of *writing* a select rather than a
second mechanism deciding what shows. It lists in the **view's** order with the
hidden columns appended, and applies in the order listed, because `:select c a`
sets an order as well as a set and listing in file order would silently undo it.
Unticking the last column is refused where it is pressed rather than on `Enter`,
after a whole session of ticking that cannot be applied. Pins go in and come back
out, and are allowed on a hidden column, since a pin waits for its column
anyway; on apply, pins the screen has no room for are **trimmed from the right
and named** rather than refused whole — the view change is what the user came
for, and giving that up over a pin would be abandoning the wrong half, so it
keeps what fits the way a filter that fills its row set keeps what it has. Unlike
`-` it needs no column cursor and works in row mode. `?` reaches it, and its help
is the picker's keys alone, since `GENERAL`'s `q` would name a key the picker
does not answer.
`Store::sort` already worked this way.

Evaluation order is fixed independently of the order commands were typed:
**filter → sort → select**, as in SQL, so a filter or sort can name a column
that is not on show.

**`:filter` does not go into the frame.** Polars pushes a projection into the
scan and lets the slice follow it down, but a filter stops the slice pushdown
dead — measured on a 400k-row CSV, the first page costs 18ms unfiltered and
101ms filtered, and that gap grows with the file rather than staying put,
because a filtered slice reads everything whatever offset it is asked for.
Scrolling would pay it on every keypress.

So a filter is resolved once into the sorted set of source rows it matches
(`data/rows.rs`), and paging becomes a gather: read the span the page covers,
take the wanted rows out of it. `Store::scan_indexed` is the chunked background
scan, shared with `/` search — both ask the same question of the file and both
want the answer progressively. It reads each chunk from the byte offset the row
index gives it: the lazy alternative re-reads from the top for every chunk, so
its cost grows with the offset and the whole scan is quadratic. Measured on a
2M-row CSV, the same filter took 4.16s in fixed 10,000-row chunks and 45.6ms
read by span. Chunk size is bounded by **bytes**, not by a row count: rows differ in
width between files by more than an order of magnitude, so the same row count is
a few megabytes in one file and gigabytes in another, and only the bytes bound
the memory. `budget::scan_bytes()` sets the ceiling and the index answers which
row that reaches, so every chunk boundary is still a checkpoint. A chunk that matched nothing still
reports, so a scan that is merely finding nothing cannot look like one that has
stalled. It buys a real row count, cancellation, and row
identity, which is what lets the edit buffer survive a filter: a row picked out
of a filtered view still knows which line of the file it came from.

**A sort is held, not redone.** Sorting cannot be lazy — nothing can know which
row comes first without reading them all — so a lazily sorted page costs a full
read of the file, *every page*. `Store::resort()` does it once instead and keeps
the whole sorted frame, which costs about what one of those pages cost: measured
on 2M rows, 102ms to sort and then 5µs for four pages. The frame carries a
`__src__` column added *before* the sort, so every displayed row knows its line
in the file — which is what lets a sorted view be edited at all.

Past what `budget::sort_cells()` allows, plv **refuses to sort at all** rather than falling back to
a lazy sort. Falling back looks like the kindness and is not: Polars has to read
and rank every row either way, so a lazy sort of a table that does not fit does
not degrade, it fails slowly. Measured against an 842M-row census parquet, the
lazy path reached 12.5GB resident in 45 seconds without producing a page; the
refusal peaks at 125MB. `Store::sort_blocked()` is asked *before* a key is
recorded, so a refusal leaves the view as it was. Lake tables sort through
DuckDB, which spills, and are not capped.

**The bounds come from `data/budget.rs`**, which asks the machine for its memory
rather than assuming one: a quarter of it for a held sort, a twentieth for a
filter's row set. Counted in cells at 128 bytes each — measured over three
shapes of real data a held sort costs 35 bytes per cell for integers, 82 for
strings and 46 for the mixed census table, while the same three against *file
size* vary more widely, so cells calibrated to the worst case is the steadier
estimate. A filter that fills its set keeps what it has, stops the scan, and
reads `(first n)` in the status bar rather than looking like the whole answer.

**A filter and a sort compose, and only one mechanism runs at a time.** With a
sort held the whole table is already in memory, so the filter is applied as part
of building that frame — `with_row_index` first so the row numbers are the
file's, then the filter, then the sort — and no row-set scan is started, because
it would be re-reading a file that has already been read. Without a sort the
scan is the cheaper answer, since it materialises nothing.

That also sharpens the sort cap: a filter that has finished resolving has
already narrowed the table, so `sort_fits` asks how many rows are on show rather
than how many the file holds. A small slice of a table far too big to sort whole
can still be sorted.

`Store` holds the `View` and converts at its own boundary: **its indices are
source columns, everything above it counts display positions.** `source_column`
/ `display_column` and `source_row` / `display_row` are the crossing points,
and the edit overlay goes through them in both directions — an edit typed into a reordered view is stored against
the file's column, and an edit on a column the view hides stays pending and is
still written by `:w`, it just has nowhere on screen to be marked.

Grammar, deliberately closed: `~` and `!~` are regex and read any column as text
(as `/` search does); other comparisons require a literal matching the column's
type; an empty literal `""` means the cells with nothing in them, matching how
plv renders and writes empty fields elsewhere. Conditions join with `and` only —
no `or` and no parentheses, because precedence cannot be introduced later
without changing what already-written commands mean.

## Completion (`src/complete.rs`, `src/ui/panel.rs`)

Tab on the `:` line completes, deliberately **not** fuzzily: this is a small
closed vocabulary — a few verbs and the file's own column names — that the user
is trying to type exactly, where a prefix match is predictable and a fuzzy one
is a guess. Fuzzy matching belongs on `/`, over the data.

What is offered depends on where in the line the cursor is: verbs at the front,
column names after `:select`/`:hide`/`:sort`, slot names after `:reset`, and
inside a `:filter` the positions cycle through column, operator, value, `and` —
a value being the file's own data, which is not a vocabulary to complete
against. A test asserts every verb completion offers is one `view::parse`
accepts, so the two cannot drift.

Candidates are matched against the bare column name and offered in the form that
has to be typed, so `rel` finds `release date` and writes `"release date"`.
Tab extends the line as far as the candidates agree and never shortens it, then
steps through them; Shift+Tab steps back.

`Panel` draws the candidates above the status bar. Unlike the `?` overlay it
sits *in* the layout — `viewport_rows` subtracts its height, so the table gives
up exactly the rows the panel takes rather than being covered. It is capped at
four rows and says `… n more` rather than truncating in silence, because a list
that quietly stops reads as the whole list.

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
