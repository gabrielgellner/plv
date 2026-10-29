# src/data — the data layer

## Overview

Everything that touches a file or a lake: detecting the format, indexing where
the rows are, fetching a page, resolving a `:filter` into rows, holding a sort,
buffering edits and splicing them back into the file, and reading DuckLake
through DuckDB. `Store` is the boundary — the app layer
([`../README.md`](../README.md)) talks to it and to nothing else in here — and
its indices are **source** rows and columns, the file's own, while everything
above it counts display positions.

The governing constraint is that files may be larger than memory. Only a page
is ever parsed for the screen; the one exception is a held sort, which is
bounded by what the machine has (`budget.rs`) and refused past it.

## Interface

- `mod.rs` — re-exports `Store` and `Overlay`.
- `store.rs` — `Store`. Opening: `open_file`, `new`, `with_row_count`,
  `new_lake`. Rows and columns: `row_count`, `source_row`,
  `search_row_to_display`, `is_added`, `columns`, `column_count`,
  `source_column`, `display_column`, `column_info`. View and sort:
  `apply_view`, `begin_sort`, `resort`, `sort_blocked`, `adopt_sorted`,
  `clear_sort`, `sort_display`, `expand`, `notes`. Paging: `scroll_to_offset`,
  `resize`, `set_view`. Editing: `edit_blocked`, `delete_blocked`,
  `insert_blocked`, `is_editable`, `edit`, `add_row`, `delete_rows`, `undo`,
  `redo`, `dirty`, `cell_text`, `block_text`, `edited_cells`, `save`. Search
  and filter: `search_async`, `begin_filter`, `extend_filter`,
  `finish_filter`, `filter_truncated`. Public fields `schema`, `total_rows`,
  `row_offset`, `viewport_rows`, `current_view`, `view`.
- `loader.rs` — `FileFormat`, `detect_format`, `load` (path → `LazyFrame`),
  `separator` (the sniffed delimiter), `Preamble` and `preamble`.
- `index.rs` — `RowIndex` (`build`, `build_lines`, `seek`, `end_of`,
  `chunk_end`, `rows`, `header`), `Building` (an index collected during a
  write), `read_span`, `STRIDE`.
- `jsonl.rs` — `Fields`, `Field`, `Kind`, `KeyPath`; `Scan` (key discovery fed
  bytes); `sample_under` and `Sample` (for `:expand`); `page`, `locate`,
  `insert_point`, `is_number`; `MAX_COLUMNS`, `SHOWN_SHARE`, `SAMPLE_RECORDS`,
  `SAMPLE_BYTES`.
- `rows.rs` — `RowSet` (a filter's matched rows, and paging by index);
  `predicate` (a checked `Filter` → `Expr`), `as_text`, `text_column`.
- `natural.rs` — `key` (the natural-order key of a value), `worth_keying`.
- `budget.rs` — `sort_cells`, `filter_rows`, `scan_bytes`.
- `edit.rs` — `Overlay` (edits, struck rows, added rows, undo/redo
  transactions, `clear`), `Cell`.
- `writer.rs` — `save` → `Saved { stamp, index }`, `save_json`, `splice`,
  `splice_json`, `JsonColumn`, `JsonType`, `encode` (field quoting, shared with
  the clipboard), `Stamp` (`of`, `still_matches`).
- `lake_db.rs` — `LakeDb` (`open`, `snapshots`, `current_snapshot`, `tables`,
  `partitions`, `source`, `count`, `page`, `reader`, `file_map`,
  `column_names`, `conn`, `try_clone`); `Snapshot`, `TableInfo`, `Partition`,
  `LakeSource`, `Reader`, `FileMap`; `detect`, `page_with`,
  `match_indices_sql`, `human_count`, `human_bytes`.

## Invariants

- `Store` speaks source indices inward and display positions outward, and
  converts only at `source_column`/`display_column` and
  `source_row`/`search_row_to_display`. Widths, pins and the overlay are keyed
  by source column, so the schema only ever grows at its end.
- The row index, the Polars reader and the writer agree about what a record is
  and where the header is (`loader::preamble` is the one answer to the latter);
  tests hold them to it.
- A page parsed from the middle of a file is given the schema already inferred;
  a lake page is cast to the table's declared columns. Types do not change as
  you scroll.
- The writer re-emits the file's own bytes and substitutes only edited fields;
  it never re-serialises a frame.
- A write that cannot account for every record, edit, added row and struck row
  is refused before the rename, and a file changed on disk since it was opened
  is refused without `:w!`.
- A held sort, and a filter's row set, never exceed what `budget.rs` allows: a
  sort past it is refused before it is recorded, and a filter that fills its
  set stops and says `(first n)`.
- The overlay is stamped onto every page `Store::fetch` returns.

## Side Effects

- Reads: `index.rs` streams the whole file once at open (and `jsonl.rs`'s key
  scan rides along); `loader.rs` reads a sample to sniff and the preamble lines;
  `read_span` seeks and reads one byte range per page or chunk;
  `jsonl::sample_under` reads up to 256MB again for `:expand`.
- Writes: `writer.rs` streams into `.<name>.plv-tmp` beside the target,
  flushes and fsyncs it (`F_FULLFSYNC` on macOS, falling back to `fsync` where
  that is unsupported), copies the original's permissions and renames over the
  target; the temp file is removed on failure. `Store::save` then adopts the
  new index, or re-reads a JSONL file.
- Threads: `Store` spawns workers for a held sort (`resort`), for filter and
  search scans (`scan_indexed`, `scan_rows`) and for lake searches; they report
  over `mpsc`, and dropping the receiver cancels them.
- DuckDB: `lake_db.rs` opens an in-memory connection, runs `INSTALL ducklake;
  LOAD ducklake` (which needs the network the first time), and attaches the
  lake `READ_ONLY`; `FileMap::page` reads the lake's Parquet files with Polars.
- `budget.rs` asks the OS for total memory once and caches it.

## Design

### Reading a file

`index.rs`: a delimited file has to be read through once to know how many rows it has, so that pass records where the rows are too — the byte offset of every `STRIDE`th row. A page then seeks to the nearest checkpoint and parses forward a bounded number of rows instead of counting from the top. On a 30GB CSV that is 46s → 3ms for the last page, and the scan holds nothing: peak memory for opening, paging, editing and rewriting that file is 57MB. The scan is quote-aware, and a test holds it, `writer.rs` and Polars to the same answer about what a record is. A page parsed out of the middle is given the schema plv already inferred, or a chunk would type its columns by whatever happens to be in those rows and the types would change as you scroll.

`loader.rs`: detects `.csv`, `.tsv`/`.tab`, `.txt`, `.jsonl`/`.ndjson` and `.parquet` by extension. Everything but JSONL opens as a `LazyFrame`; JSONL is answered by `Store::open_jsonl` before a frame is ever asked for. Delimited text goes through `LazyCsvReader` with an explicit `with_separator`. `.txt` names no delimiter, so `sniff()` picks one: it counts tab/comma/semicolon/pipe outside quoted spans on the first few lines and takes the candidate that occurs the same non-zero number of times on every line, falling back to a tab. `.csv` is sniffed too, but only between semicolon and comma, because a locale that writes `1,5` for a decimal (French, German) separates its CSV with `;`. Semicolon is tried first, since one decimal per line makes the commas line up as well, and an unsettled sample falls back to a comma. A run of `#` lines at the top of a delimited file is a **preamble**: `loader::preamble` finds it, Polars skips it by line count (`with_skip_lines`), the row index seeks past it, and the writer copies it through byte for byte — all three ask that one function, so they cannot disagree about which line is the header. Only the leading run counts, since past the header a `#` line is a record; and when the last `#` line splits into as many fields as the line after it, it is the header itself (`#chrom` in VCF and BED) and stays one. Paths are converted to `PlRefPath` for the polars API.

`store.rs`: `Store` owns the source — a `Source` with three arms: `Lazy(LazyFrame)` for delimited text and Parquet, `Json` for a JSONL file, `Lake` for a lake table — and tracks `row_offset`/`viewport_rows`. A scroll fetches only the rows on screen: out of the held sorted frame if there is one; from the byte span the row index points at for delimited text and JSONL (`fetch_span` → `indexed_span`); through `lake_db::page_with` for a lake table; and otherwise — Parquet, or a delimited view with a `:select` or a sort not yet held — as `lf.clone().slice(offset, height).collect()`. Only a held sort materialises more than a page. Also exposes `schema: SchemaRef` for column metadata, and owns the edit `Overlay` (see below).

`jsonl.rs`: a `.jsonl`/`.ndjson` file is read by **plv, not by Polars**. Polars' ndjson reader will put a nested value in a `String` column, but writes it in its own `ValueDisplay` form — `{x: 1}`, unquoted keys, documented as "not guaranteed to be valid JSON" — and the whole point of reading logs here is that `K` opens the nested field *as a document*. So a nested value is carried through as the **exact bytes of the file**, the promise `writer.rs` makes on the way out. Record boundaries are simpler than CSV's, not harder: a JSON string cannot hold a raw newline, so every `\n` ends a record and `RowIndex::build_lines` needs no quote state. **The columns are the file's true keys**, collected during that same index pass — which is already reading every byte and finishes before the first frame — rather than sampled like DuckDB's 20,480 records or discovered as they arrive like VisiData's, which moves the table sideways while you read. Keys are ordered by how many records carry them; past `MAX_COLUMNS` (1024, ClickHouse's number) they stop becoming columns and are counted out loud. A key in fewer than `SHOWN_SHARE` of the records (a fifth, Splunk's rule) opens hidden behind `C` and `:select`: a table 200 columns wide has answered no question, and what was hidden is said once when the file opens. A key whose values were all one kind is typed as it — nulls aside, and with a mix of integers and floats (or booleans and integers) widened to the wider of the two — so `:filter status > 400` compares numbers; anything that ever held an object or an array is text holding the document. A line that is not a JSON object is still a row, because the row numbers are the file's lines and dropping one would put every number after it out by one. `Store` holds no `LazyFrame` for a JSONL file, so `Source::Json` builds every page from the byte span the index points at — and `Records` is the one place that decides how a span becomes rows. `:expand col` lifts the documents in a column into columns of their own (`err` → `err.code`, `err.detail`), which is the other direction from `K`: one reads a document whole, the other compares one field of it down the file — VisiData's `(`, and its reason. It is **additive**: the new columns go on the *end* of the schema and take the parent's place in the view, because widths, pins and the edit overlay are all keyed by source column index and inserting would quietly move somebody's pin. For the same reason there is no un-expand — `-` hides a column, and removing one would renumber everything after it. A column's value is found by a **`KeyPath`**, not by splitting its name on `.`: a JSON key may contain a dot, and `err.code` would then be a guess about which of the two it was. Discovery is a second pass over the file (unlike the key scan at open, which rides along with the row index), so it is **bounded** — 20,480 records that carry the column, DuckDB's `sample_size`, and a hard 256MB (`SAMPLE_RECORDS`, `SAMPLE_BYTES`) — and says when a bound stopped it, as `(from the first 20480 records with it)`. The message names the record bound even when it was the byte bound that stopped the read. `view::Command::Expand` is parsed and checked like every other command, so a bad column name is underlined at the prompt, but it writes to no slot of the `View`: what columns *exist* is the store's business, and `App::apply_view_command` sends it there before the view is asked.

A **sort** is the one thing that needs the whole table, so `json_frame` reads the file once in byte-bounded chunks and hands the result to the same `materialise` a delimited sort uses, under the same `budget::sort_cells` cap: 220ms and 350MB for a 62MB, 500k-line log, and 4µs a page after. While that read is in flight a page is still the file's own order — there is no lazy sort to stand in for it as there is for a CSV — which lasts as long as the spinner does.

### Editing

Delimited text — `.csv`, `.tsv`, `.tab`, `.txt` — and JSONL can be edited in
place. Parquet and lake tables stay read-only: Parquet is genuinely typed, so a
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

**A JSONL record is spliced the same way**, a line at a time rather than a byte
at a time — a JSON string cannot hold a raw newline, so no record spans two
lines. `jsonl::locate` gives the byte range of the value the cell was read from
and the writer puts the new one in its place, leaving key order, spacing and
escaping alone; a record with nothing to change is copied verbatim. A value is
written as a number or a boolean where the typed text will pass as one and as a
JSON string otherwise, which is the delimited rule ("a value that does not fit
leaves the column as text") in JSON's terms; an empty cell is `null`, since that
is what plv draws as `·` either way. A key the record lacks is **added at the
end of the record**, but only at the top level: one further in would mean
inventing the documents above it, so it is refused and the write says so. An
added row is a document built from the cells that were typed, nesting included,
so a new row can be filled in through `:expand`ed columns. Reloading after a
JSONL write does more than the delimited one — the index, the row count and the
keys are all read again, since a deleted record moves every record after it —
and it puts the view back **by name** and re-appends the `:expand`ed columns,
which are derived rather than found and would otherwise vanish along with the
edit just written through them.

**The write hands back the index of the file it wrote.** After a `:w` the row
count and every byte offset describe a file that no longer exists: a deleted
record is gone, an added one is new, and an edit that changed a field's length
moved everything after it. Rebuilding the index by reading the file again would
cost a full pass after every write — 31s on the 28GB CSV — so the splicer notes
it on the way past instead, since it is already walking every record of its own
output. `index::Building` collects the checkpoints and `writer::Saved` carries
them back beside the `Stamp`; `Store::save` adopts both, but only when it wrote
to the file it read from, since `:w path` leaves this buffer describing its own
file. That is also why every byte the splicer emits goes through `write_out`
and not to the writer directly — it is the one place that counts what the new
file is made of, and a replacement that went around it left the offsets after
it short by its own length.

Four guards run before the rename, in both the delimited and the JSONL
splicer: the record count must match the row count the store opened with
(`total_rows`, the file's own count rather than what a filter shows), every
edit must have found a field to land in (which catches ragged rows), every added
row must have found the row it goes ahead of, and every struck row must have
been found to drop. The output is fsynced beside the target and moved into place, so a failed
write leaves the original intact. An open-time `Stamp` (length + mtime) refuses a
write to a file that changed underneath the buffer; `:w!` forces.

**Polars does not skip blank lines** — it reads an empty line as a row of nulls,
trailing ones included — so every line ending closes a record in the writer too.
Getting this wrong puts edits one line off; `record_numbering_agrees_with_the_polars_reader`
pins it against the real reader rather than against the assumption.

**Editing waits for a sort to be held.** A sorted page's rows are not the
file's rows, so an edit can only be told which line it belongs to once the
sorted frame has arrived with its `__src__` column (see *A sort is held*,
below); until then the edit is refused as `still sorting`. Deleting rows stays
refused under a sort or a filter, for the reason given above, and so does adding
them (`Store::insert_blocked`). A file plv cannot write — Parquet, a lake
table — refuses every edit.
`Store::edit_blocked()` returns the reason as a string, so the rule and its
explanation cannot drift apart.

### Filters and sorts

The commands that ask for these are parsed and checked in [`../view.rs`](../README.md#the-view-language-viewrs); this is how they are run.

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

**A text column sorts by the numbers written in it.** Byte order puts `100%`
before `23%` before `9%`, because the comparison stops at the first character
and never gets as far as noticing that one of the three is a hundred — and the
same goes for `file10` against `file2` and `v1.10` against `v1.9`. So
`data/natural.rs` builds a key that emits every digit run behind a marker
counting its digits, which makes byte order over the key the natural order over
the value.

It is **on by default rather than behind a flag**, because of *when* it differs
from byte order: only where two digit runs reached by the same text are of
different lengths. Runs of equal width — a hash, a zero-padded id, an ISO date,
a uuid — compare identically either way, so the rows it moves are exactly the
rows byte order was getting wrong, and there is nothing to weigh up at the
prompt. `natural::worth_keying` asks that question over the column in one pass
without building a key (it keeps one counter per run position), so a date column pays for nothing. It asks it *per run
position* and in **written** characters rather than significant digits: a date
holds runs of 4, 2 and 2 within one value, and `01` against `12` is uneven as
numbers and perfectly ordered as text.

What it costs, measured over a held sort of 2M rows: a numeric column is
untouched (108ms before, 111ms after); a text column that needs a key goes from
146ms to 243ms; and a text column that does not — the zero-padded case — pays
17ms for the pass that says so, 155ms to 172ms. The key column is transient,
built between the `hstack` and the `drop_many` and never held, and the 128
bytes a cell is budgeted at is already the worst case against a measured 35 to
82, so it is not counted against `budget::sort_cells` separately.

**It orders, it does not type.** The column stays text, `:filter share > 50`
refuses it exactly as before, and the cell is drawn as the bytes it holds — the
only thing that changed is which row comes first, which is a fact about the sort
and not about the file. Reading `83%` as a number is a different feature and a
much larger one.

The key is a **column, not an expression**, because Polars' Rust lazy API has no
way to say "order by this function of the value". `Store::materialise` collects
first — which a held sort does anyway — then `sort_naturally` builds the key
beside the column it orders and drops it again, so nothing above the store ever
sees it. Two paths deliberately keep byte order and say so: the lazy pipeline in
`effective_lf`, which only draws the window between a sort being asked for and
the frame arriving (already provisional — it carries no `__src__`, and a JSONL
file spends that same window in the file's own order), and a lake table, which
is sorted by DuckDB page by page precisely because it is the one source too
large to hold a key for.

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

### Source columns and display positions

`Store` holds the `View` and converts at its own boundary: **its indices are
source columns, everything above it counts display positions.** `source_column`
/ `display_column` and `source_row` / `search_row_to_display` (through
`RowSet::display` under a filter) are the crossing points,
and the edit overlay goes through them in both directions — an edit typed into a reordered view is stored against
the file's column, and an edit on a column the view hides stays pending and is
still written by `:w`, it just has nowhere on screen to be marked.

### DuckLake (`lake_db.rs`)

The browser screens and keys are in [`../README.md`](../README.md#the-ducklake-browser-lakers).

**Lake data is read through DuckDB's `ducklake` extension**, not by plv. The
extension is DuckLake's reference reader and presents the *logical* table:
inlined rows merged, delete files applied, schema evolution and column mapping
handled. Reading the Parquet files directly would mean re-implementing all of
that against a 28-table spec — plv did that once, and it leaked (inlined rows
went missing, delete files were ignored). CSV and Parquet still go through
Polars; `Store` carries a `Source` enum with an arm for each kind of reader —
`Lazy` (Polars: delimited text and Parquet), `Json` (plv's own JSONL reader)
and `Lake`.

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
- **Deep paging seeks rather than counts.** `LIMIT n OFFSET m` makes DuckDB
  produce and discard every row up to the offset: 2.9s for the last page of the
  1.14B-row census table against 137ms for the first. But the rows are in
  parquet files and the catalog records each file's `record_count`, so
  `LakeDb::file_map` reads that arithmetic once and `FileMap::page` reads the
  page out of the file it falls in with Polars, which seeks by row group —
  0.6ms for that same last page, 140MB peak. The reader is still DuckDB's; this
  only answers *where*, which is what makes it safe where re-implementing the
  format was not. It is **entirely optional**: a delete file, a column mapping,
  an ambiguous path, a file short of its stated rows, or file counts that do not
  add up to `count(*)` all return `None` and leave the query to go the way it
  always did. That last check is the one doing most of the work — deletes,
  inlined rows and a mis-read snapshot are all caught by arithmetic rather than
  by a list of conditions. A sort or a partition filter never takes the path:
  one asks for an order no file holds, the other for which files answer a
  `WHERE`, which is a question about physical layout that plv deliberately does
  not ask.
- **A page is cast to the shape the table declares** (`Reader::columns`, from
  `DESCRIBE`), not to what its own values suggest. A page used to be typed by
  the first non-null value *in that page*, so a column empty here and numeric
  three screens down changed type as you scrolled — the same drift the
  delimited path avoids by handing each chunk the schema. It is also what lets
  the two paging paths agree exactly, which is the property that makes the fast
  one substitutable.
- **Partition lists** cost a `GROUP BY` (~0.5s on that table), so they load on
  demand rather than up front.
- **Background work** clones the connection with `try_clone()`, which shares the
  attached lake instead of paying the ~20ms ATTACH again.
- **Type mapping.** Page results are converted to Polars columns; integers,
  floats and booleans keep their type, everything else renders as text.

## Verification

- Run: `cargo test` — in particular
  `cargo test record_numbering_agrees_with_the_polars_reader`, which holds the
  writer's record numbering to the real Polars reader.
