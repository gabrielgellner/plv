# plv

A terminal viewer and editor for CSV, TSV, JSONL, Parquet and [DuckLake](https://ducklake.select/) data, inspired by [csvlens](https://github.com/YS-L/csvlens). Built with [Polars](https://pola.rs/) and [ratatui](https://ratatui.rs/).

- Supports CSV, tab-separated text, JSONL logs, Parquet, and DuckLake lakes
- **Edits delimited text and JSONL** — cells, blocks, whole rows — in a buffer, written with `:w`
- **Reads a cell in a window** — `K` — with JSON, XML, HTML and markdown recognised and laid out
- **Yanks to the system clipboard** as TSV, and takes a paste back from it — no clipboard library, and it works over SSH
- **A view language** — `:select`, `:hide`, `:filter`, `:sort`, `:expand` — with Tab completion over the file's own column names
- **Column control** — pin columns to the left edge, hide one with a keystroke, or pick from a list
- Works on files larger than memory: a 30GB CSV opens, pages anywhere, edits and writes back within about 60MB
- Browse a lake's tables, partitions and snapshots — including time travel
- Vim-style navigation throughout

## Usage

```
plv <file.csv>
plv <file.tsv>          # also .tab; .txt sniffs its delimiter
plv <file.jsonl>        # also .ndjson: one JSON object per line
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

## Reading a cell

A column can only be made so wide, so there are two ways to see a value that
does not fit. `K` — vim's "tell me about the thing under the cursor" — opens the
cell in a window over the table: wrapped at word boundaries, titled with the
column, its type and its size, and scrolled with `j`/`k`, `Ctrl+d`/`Ctrl+u` and
`g`/`G`. It holds the keys while it is up, so `q` closes it rather than quitting
plv, and it covers the table rather than taking rows from it — closing it puts
the screen back exactly as it was.

A cell that is a **document** is shown as one: re-indented, coloured, and titled
`note — json, 291 characters` or `body — html, 209 characters`. JSON, XML, HTML
and markdown are recognised.

Detection is a real parse rather than a guess — a value is JSON only if it parses
all the way to the end, and markup only if every tag closes in the right order,
so prose with a `{` or a `<` in it stays prose. The document is re-indented from
its **own bytes** rather than rebuilt from a parsed model, so JSON's key order,
number formatting and duplicate keys survive, and so do markup's attribute
quoting and entities. `r` switches to the raw value and back, since the formatted
view is an interpretation and the raw one is what gets edited and written.

Markdown is the odd one: it was designed to be read as it is written, so there
is nothing to re-indent and plv only says which parts are which — headings bold,
fences in the colour code is drawn in, a bullet's dash out of the way of the
words after it. **Every marker stays where it was**, drawn dim rather than
hidden, so the text on screen is the text in the cell character for character.
Its detection is the one judgement rather than a parse, since a paragraph is a
valid markdown document: what is asked is whether there is structure worth
drawing — a heading, a fence, a quote, or more than one list item. Getting that
wrong costs a dim dash, because nothing is hidden either way.

Markup has to be well-formed. Real HTML often is not — an unclosed `<p>`, an
`<li>` left hanging — and those cells stay text, deliberately: a half-parsed
document drawn as a tree is a claim about where things nest, and getting that
wrong is worse than not indenting at all. Void elements (`<br>`, `<img>`) are the
exception, since closing themselves is their rule; needing that rule, or a bare
or unquoted attribute, is what makes a document `html` rather than `xml`. An
element holding text is left on one line — breaking prose apart to show
structure would be showing structure that is not there.

`zk` is the other half: the same value in the strip above the status bar, two or
three lines of it, staying on as the cursor moves so a column of long values can
be read by walking down it. One is for reading a cell, the other for scanning a
column of them. Newlines inside a quoted field are kept as the author wrote
them by both.

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

CSV, TSV, `.txt` and JSONL files can be edited. Parquet stays read-only — it is
genuinely typed, so a one-cell change would mean rewriting the whole file
against a schema — as do lake tables.

| Key | Action |
|-----|--------|
| `i` / `a` | Edit the cell, caret at the start / end |
| `c` | Replace the cell |
| `x` | Clear the cell |
| `dd` / `{n}dd` | Delete the row, or n rows |
| `o` / `O` | Open a new row below / above |
| `y` | Yank, to the register and the system clipboard |
| `p` | Paste the register at the cursor |
| `u` / `Ctrl+r` | Undo / redo |
| `:w` `:w!` `:w path` | Write (force past a changed file / write elsewhere) |
| `:q` `:q!` `:wq` | Quit (discarding / writing) |

**Nothing reaches the file until `:w`.** Edits live in a buffer keyed by
position in the file, so memory follows the number of changes rather than the
size of the file, `u` reaches all of it, and `q` refuses while anything is
pending. The status bar carries a `[+n]` count and edited cells are drawn in
red.

**`y` reaches the system clipboard**, as TSV — which is what a spreadsheet puts
on the clipboard when you copy a range, and what it expects to receive. Coming
back the other way, your terminal's own paste key drops a block in at the
cursor: plv reads it as TSV, so a range copied from a spreadsheet keeps its
shape, and anything past the last row or column is dropped with a count rather
than wrapping. `p` still pastes plv's own register.

Neither direction needs a clipboard library. Going out is OSC 52, an escape
sequence the terminal answers, so it works over SSH where a native clipboard
would be reaching for the wrong machine's; coming in is bracketed paste, which
is the terminal handing plv what you pasted. Yanks larger than 64kB are kept in
the register but not sent, since a terminal that quietly truncates the sequence
would leave half a block on the clipboard with nothing about it looking wrong.
Inside tmux, `set -g set-clipboard on` is what lets the sequence through.

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
| `y` | Yank the selection, to the clipboard as well |

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
| `:expand payload` | Lift a JSONL document into columns (see below) |
| `:reset [slot]` | Clear select, filter, sort, or all of them |

A verb on its own puts its slot back, so `:select` shows every column again and
`:sort` returns the file's own order. `:expand` has no bare form: it changes
which columns exist rather than which are on show, so there is nothing for it to
put back — `-` hides a column it made.

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

## JSONL

Point plv at a `.jsonl` or `.ndjson` file and each line's top-level keys become
columns.

```
plv app.log.jsonl
```

**The columns are the file's real keys, not a sample of them.** plv already
reads every byte of a file when it opens one, to count the rows; the same pass
collects the keys, so the column set is exact and settled before anything is
drawn. Columns are ordered by how many records carry them, most common first.

**Rare keys are available but not shown.** A key in fewer than a fifth of the
records is a column you can reach with `C` or `:select`, but the table does not
open two hundred columns wide to hold it. plv says what it left out when the
file opens; `:reset select` shows every key.

**A key that only ever held one kind of thing is typed as it.** A `status` of
whole numbers is a number column, so `:filter status > 400` compares numbers and
not text. A key that ever held an object or an array is text — and that text is
**the file's own bytes**, so `K` opens it as the document it is:

```
 level    service    msg              job
 error    worker     job failed       {"id":"j-9f21","kind":"reindex","attempt":2}
                              ┌ job — json, 44 characters ┐
                              │ {                         │
                              │   "id": "j-9f21",         │
                              │   "kind": "reindex",      │
                              │   "attempt": 2            │
                              │ }                         │
                              └ r raw   q close ──────────┘
```

Only top-level keys become columns; anything deeper stays inside its value,
where `K` reads it. A line that is not a JSON object is still a row — dropping
it would put every row number after it out by one — it simply has no fields.

`:expand err` lifts the documents in a column out into columns of their own —
`err.code`, `err.detail`, `err.retryable` — which is how you compare one field
of a nested object down the file rather than opening each one with `K`:

```
 level   service   err.code   err.detail                     err.retryable
 error   api       502        connection reset by peer       true
 error   worker    500        deadlock detected              true
```

The children take their parent's place in the view, and the parent stays a
column — `:reset select` or `C` brings it back, and `K` still opens it. The keys
come from reading the file, bounded at 20,480 records carrying the column, and
plv says when a bound is what stopped it rather than letting a partial answer
look complete. There is no un-expand: `-` hides a column, and taking one out of
the schema would renumber the columns that widths and pins are keyed by.

`s` sorts, the same way a CSV sort works: the file is read once into a frame
that is then held, so the pages after it are free. On a 62MB, 500,000-line log
that is 220ms to sort and 4µs a page, in 350MB of memory — and past what there
is memory for, plv refuses rather than trying. `:filter` composes with it, and
both `:filter` and `/` pay the same linear scan a CSV does.

JSONL files are **editable**, and written the same way a CSV is: the record's
own bytes are streamed through and only the edited value is replaced, so key
order, spacing, escaping and every other record survive exactly. A value is
written as a number or a boolean when the text will pass as one and as a string
otherwise; an empty cell is `null`. A key the record does not have is added at
the end of it — but only at the top level, since adding one further in would
mean inventing the documents above it, and that is refused with a reason rather
than guessed at. `dd` and `o` work too: a struck record leaves no blank line
behind, and a new one is a document built from the cells you typed, nesting
included.

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

Paging into a lake table used to cost `LIMIT n OFFSET m`, which makes DuckDB
produce and discard every row up to the offset: 2.9 seconds for the last page of
a 1.14-billion-row table. The rows are in parquet files, though, and the catalog
records how many rows each file holds — so plv reads that arithmetic once when a
table is opened and a page then seeks to the file its rows are in. The same page
now costs under a millisecond, and the whole table pages in tens of
milliseconds.

The reader is still DuckDB's; this only ever answers *where*. It is also
entirely optional: if the catalog will not say plainly — a delete file, rows
inlined in the catalog, a column mapping, or file counts that do not add up to
`count(*)` — plv silently pages the way it always did. A sort or a partition
view goes to DuckDB too, since neither is a question about where rows sit in a
file.

Lake tables are read through DuckDB's `ducklake` extension — DuckLake's own
reference reader. That means what you see is the **logical** table: rows that
DuckLake has inlined into the catalog database are included, delete files are
applied, and schema evolution is handled by the format's implementation rather
than by plv. CSV, TSV and Parquet files still go through Polars, and JSONL
through plv's own reader.

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

| | 28GB CSV, 272M rows | 800MB parquet, 842M rows | DuckLake, 1.14B rows | 62MB JSONL, 500k lines |
|---|---|---|---|---|
| open | 31s | 55ms | 160ms | 157ms |
| page anywhere | 3–7ms | <1ms | 0.4–34ms | 2–3ms |
| peak memory | 63MB | 126MB | 140MB | 30MB |

Opening a delimited file reads it once, to count the rows — and that pass also
records where the rows are, so a page afterwards seeks to the nearest checkpoint
instead of counting from the top. A JSONL file is read the same way, and the
same pass collects the file's keys, so its columns are exact rather than
sampled. Parquet needs no such thing: it can already seek by row group.

Sorting is the one operation that has to hold the table, since nothing can know
which row comes first without reading them all. plv sorts once and keeps the
result, and declines outright when the table is larger than the memory it would
take — the bounds come from the machine rather than being compiled in.

## Install

From [crates.io](https://crates.io/crates/plv):

```
cargo install plv
```

This compiles DuckDB from source, so the first build takes a while.

### Prebuilt binaries

Each [release](https://github.com/gabrielgellner/plv/releases) carries tarballs
for macOS arm64 and Linux x86_64/arm64, alongside a `SHA256SUMS` file. Windows
is not built; nothing in plv is known to prevent it, it simply is not tested.

```
tar xzf plv-<version>-<target>.tar.gz
./plv-<version>-<target>/plv --version
```

### From a checkout

```
cargo install --path .
```

`just install` does the same and then prints which version landed on your PATH.

## Build

```
cargo build --release
```
