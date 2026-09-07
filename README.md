# plv

A terminal viewer and editor for CSV, TSV, Parquet and [DuckLake](https://ducklake.select/) data, inspired by [csvlens](https://github.com/YS-L/csvlens). Built with [Polars](https://pola.rs/) and [ratatui](https://ratatui.rs/).

- Supports CSV, tab-separated text, Parquet, and DuckLake lakes
- **Edits delimited text** — cells, blocks, whole rows — in a buffer, written with `:w`
- **A view language** — `:select`, `:hide`, `:filter`, `:sort` — with Tab completion over the file's own column names
- **Column control** — pin columns to the left edge, hide one with a keystroke, or pick from a list
- Works on files larger than memory: a 30GB CSV opens, pages anywhere, edits and writes back within about 60MB
- Browse a lake's tables, partitions and snapshots — including time travel
- Vim-style navigation throughout

## Usage

```
plv <file.csv>
plv <file.tsv>          # also .tab; .txt sniffs its delimiter
plv <file.parquet>
plv <lake.ducklake>      # a DuckLake catalog
plv <bundle-dir>/        # a directory containing one
```

Press `?` at any time for the key bindings of whatever screen you are on.

## Navigation

| Key | Action |
|-----|--------|
| `j` / `↓` | Move cursor down |
| `k` / `↑` | Move cursor up |
| `Ctrl+d` / `Ctrl+u` | Half a screen down / up |
| `Ctrl+f` / `Ctrl+b` | A whole screen down / up |
| `PageDown` / `PageUp` | The same |
| `gg` / `Home` | Jump to first row |
| `G` / `End` | Jump to last row |
| `{n}gg` / `{n}G` | Jump to row n |
| `h` / `l` | Move a column left / right |
| `{n}h` / `{n}l` | Jump n columns |
| `H` | Jump to first column |
| `0` / `$` | Scroll so the first / last column sits at its edge |
| `Tab` | Cycle selection mode: row → column → cell |
| `s` | Sort by cursor column; toggles asc ↔ desc; add more columns for multi-sort |
| `zz` / `zt` / `zb` | Centre / top / bottom cursor in view |
| `#` | Relative or absolute row numbers |
| `z>` / `z<` | Widen / narrow the cursor column |
| `z_` | Fit the column to the widest value on screen |
| `z=` | Put every column width back |
| `zp` / `z\|` | Pin the cursor column to the left edge / unpin every column |
| `-` | Hide the cursor column |
| `C` | Open the column picker |
| `K` | Open the cursor cell in a window of its own (`r` raw ↔ formatted) |
| `zk` | Show the cursor cell in full, above the status bar |
| `?` | Show key bindings for the current screen |
| `q` | Quit |

A column can only be made so wide, so there are two ways to see a value that
does not fit. `K` — vim's "tell me about the thing under the cursor" — opens the
cell in a window over the table: wrapped at word boundaries, titled with the
column, its type and its size, and scrolled with `j`/`k`, `Ctrl+d`/`Ctrl+u` and
`g`/`G`. It holds the keys while it is up, so `q` closes it rather than quitting
plv, and it covers the table rather than taking rows from it — closing it puts
the screen back exactly as it was.

A cell that is a JSON document is shown as one: re-indented, coloured, and
titled `note — json, 291 characters`. Detection is a real parse rather than a
guess — a value is JSON only if it parses all the way to the end, and anything
else is left as text — and the document is re-indented from its own bytes rather
than rebuilt from a parsed model, so key order, number formatting, duplicate keys
and escapes are exactly what the file says. `r` switches to the raw value and
back, since the formatted view is an interpretation and the raw one is what gets
edited and written.

`zk` is the other half: the same value in the strip above the status bar, two or
three lines of it, staying on as the cursor moves so a column of long values can
be read by walking down it. One is for reading a cell, the other for scanning a
column of them.

Newlines inside a quoted field are kept as the author wrote them by both.

Widening a column pushes the ones after it along and off the right edge, as a
spreadsheet does, rather than squeezing everything to make room — `h` and `l`
reach what went past. Widths are remembered for the session and belong to the
column, so they survive `:select` reordering it; `z=` puts them all back.

A wide table is read by scrolling sideways, and the column saying *which row
this is* is the first to leave the screen. `zp` pins the cursor column to the
left edge, where it stays while the rest scroll past it; `z|` unpins the lot.
Pins need not be neighbours — pinning an id and a status brings two columns from
opposite ends of the file into one view, which is the case the feature exists
for. Like widths they belong to the column and survive a `:select`, and a pin
the screen has no room for is refused rather than drawn, since a table with no
room left to scroll in stops answering `h` and `l` with nothing to say why.

Row numbers count from the cursor by default, the way nvim's hybrid
`number` + `relativenumber` gutter does, so `3j` and `12G` can be read off
rather than worked out. `#` switches to plain absolute numbering.

`Ctrl+d` and `Ctrl+u` move the view and the cursor together, keeping the cursor
at the same height in the window — as vim does, rather than walking the cursor
to the edge first.

## Selection modes

Press `Tab` to cycle through three selection modes:

- **Row** (default) — entire cursor row is highlighted; search covers all columns
- **Column** — the current column is highlighted; search covers only that column; press `s` to sort, `Esc` to clear all sorts
- **Cell** — only the cursor cell is highlighted; search covers only the current column; press `s` to sort, `Esc` to clear all sorts

The mode also decides the shape of a `v` selection. Any key that acts on *a
column* — an edit, `s`, `-` — adopts a column cursor when pressed in row mode
rather than doing nothing, taking the leftmost visible column, which is where
`Tab` would have put it. So none of them is stuck behind a mode switch in the
mode plv opens in.

## Search

| Key | Action |
|-----|--------|
| `/` | Open search prompt |
| `n` | Jump to next match |
| `N` | Jump to previous match |
| `Esc` | Clear active search |

Type a regex pattern after `/` and press `Enter`. In Row mode, search covers all columns. In Column or Cell mode, search is scoped to the selected column. Matches are highlighted in the table and the status bar shows progress (`/pattern [2/15]`).

## Editing

CSV, TSV and `.txt` files can be edited. Parquet stays read-only — it is
genuinely typed, so a one-cell change would mean rewriting the whole file
against a schema — as do lake tables.

| Key | Action |
|-----|--------|
| `i` / `a` | Edit the cell, caret at the start / end |
| `c` | Replace the cell |
| `x` | Clear the cell |
| `dd` / `{n}dd` | Delete the row, or n rows |
| `o` / `O` | Open a new row below / above |
| `y` / `p` | Yank the cursor / paste at the cursor |
| `u` / `Ctrl+r` | Undo / redo |
| `:w` `:w!` `:w path` | Write (force past a changed file / write elsewhere) |
| `:q` `:q!` `:wq` | Quit (discarding / writing) |

**Nothing reaches the file until `:w`.** Edits live in a buffer keyed by
position in the file, so memory follows the number of changes rather than the
size of the file, `u` reaches all of it, and `q` refuses while anything is
pending. The status bar carries a `[+n]` count and edited cells are drawn in
red.

**Writing splices bytes rather than re-serialising.** Only the fields you
changed are replaced: quoting style, line endings, a BOM, a missing final
newline and every untouched line survive exactly. Changing one cell of a 30GB
file changes as many bytes as the value grew by, and nothing else — which is
what keeps CSV-in-git diffs readable.

### Visual mode

`v` starts a selection whose shape follows the `Tab` mode: whole rows, whole
columns, or a rectangle.

| Key | Over a selection |
|-----|------------------|
| `c` | Replace every cell with one value |
| `i` / `a` | Prepend / append text to every cell |
| `x` | Clear the selected cells |
| `d` | Delete the selected rows |
| `y` | Yank the selection |

`v jjj a` then `kg` turns a column of `1 2 3` into `1kg 2kg 3kg`. A fill is one
undo step however many cells it touched.

## Shaping the view

`:` opens a command line for narrowing what is on screen.

| Command | Action |
|---------|--------|
| `:select a b` | Show only these columns, in this order |
| `:hide a b` | Drop these columns |
| `:filter count > 10` | Keep matching rows |
| `:filter a = x and b ~ y` | Conditions join with `and`; `~` is a regex |
| `:sort a b-` | Sort by columns; `-` reverses one |
| `:reset [slot]` | Clear select, filter, sort, or all of them |

A verb on its own puts its slot back, so `:select` shows every column again and
`:sort` returns the file's own order.

**Tab completes** against the verbs and the file's own column names — including
the quoting, so `rel⇥` writes `"release date"`. Candidates appear in a panel
above the status bar; Tab steps through them, Shift+Tab back.

Commands are checked as you type them, against the schema: `:filter count > abc`
says so at the prompt, naming the column and its type, rather than failing later
inside a query.

### Choosing columns without typing

`-` hides the cursor column: `:hide <name>` without the name. It accumulates, so
pressing it again narrows further, and `:reset select` brings everything back.
In row mode it takes a column cursor first, as `s` and the edit keys do.

`C` opens the **column picker** — every column in a list, with a tick for shown
and a tick for pinned. It touches no column cursor at all, being a list of every
column rather than an operation on the one you are on.

| Key | Action |
|-----|--------|
| `j` / `k` (`↓` / `↑`) | Move down the list |
| `Ctrl+d` / `Ctrl+u` | Half a page down / up |
| `g` / `G` (`Home` / `End`) | First / last column |
| `-` | Show or hide this column |
| `a` / `A` | Show every column / hide all but this one |
| `p` | Pin or unpin this column |
| `Enter` | Apply |
| `Esc` | Cancel, changing nothing |

It holds a working copy, so `Esc` costs nothing and `Enter` is the only thing
that changes the view — and what it applies is a `:select`, so the picker is a
way of *writing* one rather than a second thing deciding what shows.

Picking four columns out of two hundred is `A` and then three ticks. The list is
in the view's own order with the hidden columns appended, and applies in that
order, so a `:select c a` survives a round trip through it rather than being
quietly put back into file order.

`q` is deliberately not a picker key. Everywhere else in vim it closes a window,
and a window is a view — closing one never destroys work — so it is not borrowed
here for something that would discard everything ticked.

## DuckLake

Point plv at a `.ducklake` catalog — or a directory containing one — and it opens
a **catalog browser** instead of a single table.

```
┌ cenprof-2021 — 2 tables @ snapshot 6 ───────────────────────────────────────────────────┐
│Table                              Rows           Size    Files  Partitioned by  Inlined │
│cenprof_2021.geography_attributes  79,176         1.3 MB  1      —               689     │
│cenprof_2021.observations          1,140,560,738  1.2 GB  22     GEO_LEVEL       —       │
```

`Enter` opens a table in the normal viewer. `l` or `f` descends into that table's
**partitions**, so you can scope the view to one partition instead of scanning
the whole table — which matters when one partition is most of the data:

```
│GEO_LEVEL=DisseminationArea            842,209,475   73.8%  │
│GEO_LEVEL=CensusTract                  90,840,592    8.0%   │
│GEO_LEVEL=Country                      14,545        0.0%   │
```

### Keys

| Key | Action |
|-----|--------|
| `Enter` | Open the selected table, file, or snapshot |
| `l` / `f` | Show the selected table's partitions |
| `a` | Open the whole table (from the partition list) |
| `T` | Show snapshots |
| `h` / `Esc` | Back |
| `b` | Back to the catalog (from the viewer) |

### Time travel

`T` lists the lake's snapshots with their timestamps, schema version, and what
changed in each. `Enter` re-resolves the catalog at that snapshot: row counts,
sizes, partitioning, and the table list itself all reflect the lake as it was.
If the table you were viewing still exists there it is reopened by name, so you
can step through snapshots watching the same table change.

A file-level scope widens to the whole table when you travel, because file ids
are not stable across snapshots.

### How lake data is read

Lake tables are read through DuckDB's `ducklake` extension — DuckLake's own
reference reader. That means what you see is the **logical** table: rows that
DuckLake has inlined into the catalog database are included, delete files are
applied, and schema evolution is handled by the format's implementation rather
than by plv. CSV, TSV and Parquet files still go through Polars.

Notes:

- The lake is attached **read-only**. If another process holds it open for
  writing, plv says so rather than waiting.
- The first lake you open needs network access, once: DuckDB fetches the
  `ducklake` extension and caches it under `~/.duckdb/extensions/`.
- If a bundle has been copied since it was built, the data path recorded inside
  it no longer exists; plv falls back to the data directory sitting beside the
  catalog file.
- Columns are shown with their DuckDB types where they map cleanly to numbers or
  booleans; dates, timestamps and other types are displayed as text.

## Large files

plv is built for files that do not fit in memory. Measured on a census extract:

| | 28GB CSV, 272M rows | 800MB parquet, 842M rows | DuckLake, 1.14B rows |
|---|---|---|---|
| open | 31s | 55ms | 150ms |
| page anywhere | 3–7ms | <1ms | 0.14–2.9s |
| peak memory | 63MB | 126MB | 329MB |

Opening a delimited file reads it once, to count the rows — and that pass also
records where the rows are, so a page afterwards seeks to the nearest checkpoint
instead of counting from the top. Parquet needs no such thing: it can already
seek by row group.

Sorting is the one operation that has to hold the table, since nothing can know
which row comes first without reading them all. plv sorts once and keeps the
result, and declines outright when the table is larger than the memory it would
take — the bounds come from the machine rather than being compiled in.

## Install

```
cargo install --path .
```

From a checkout, `just install` does the same and then prints which version
landed on your PATH.

## Build

```
cargo build --release
```
