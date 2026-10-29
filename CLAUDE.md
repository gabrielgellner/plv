# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`plv` is a terminal UI viewer and editor for CSV, TSV, JSONL and Parquet files, and a browser for [DuckLake](https://ducklake.select/) catalogs, built with Rust. It uses [Polars](https://pola.rs/) for lazy data loading (larger-than-memory files) and [ratatui](https://ratatui.rs/) + crossterm for the TUI. The goal is a csvlens-like viewer with vim navigation.

## Commands

```bash
cargo build
cargo build --release      # optimized: LTO + strip
cargo run -- path/to/file.csv
cargo run -- path/to/file.jsonl        # or .ndjson: one JSON object per line
cargo run -- path/to/file.parquet
cargo run -- path/to/bundle/          # DuckLake bundle (dir containing *.ducklake)
cargo run --example lakedump -- path/to/bundle/   # dump what the catalog reader sees
cargo test
cargo test <test_name>     # run a single test
```

## Where the design lives

Every decision in plv is written down beside the code it governs, with the
reason for it and, where there was one, the measurement that settled it. This
file is the map and the principles; the arguments are in the directory READMEs:

| Read | For |
|---|---|
| [`src/README.md`](src/README.md) | the app layer: the event loop and key bindings, the view language (`:select`/`:hide`/`:filter`/`:sort`/`:expand`), `-` and the column picker, completion, the clipboard, the DuckLake browser screens, Polars API notes |
| [`src/data/README.md`](src/data/README.md) | reading files (the row index, format detection, the preamble, JSONL), paging, how a filter and a sort are run and bounded, natural ordering, editing and the byte-preserving writer, DuckLake access |
| [`src/ui/README.md`](src/ui/README.md) | the table widget (widths, pins, the gutter, the sort indicator), the `zk` strip and the `K` cell window with its search, documents in a cell (JSON, XML/HTML, markdown), the panel, help and status bar |
| [`examples/README.md`](examples/README.md) | `lakedump` |

When you change how something works, change the paragraph that says why, in the
README beside it — and keep the number if there was one.

## Architecture (3 layers)

```
src/
  main.rs         CLI (clap), terminal init — thin wrapper over the `plv` lib
  lib.rs          library root (bin and examples both use it)
  app.rs          App layer: event loop, state, key bindings, two screens
  lake.rs         DuckLake browse state: tables ↔ partitions ↔ snapshots, and the open scope
  picker.rs       the column picker: what is on show and what is pinned
  search.rs       `/` state: the pattern, its matches, and where the cursor is
  clipboard.rs    the system clipboard: OSC 52 out, bracketed paste in
  data/
    loader.rs     detect format by extension, sniff the delimiter, find the preamble
    store.rs      scroll state + data fetching (Polars, JSONL or lake-backed), sorts, edits
    index.rs      where each row starts, so a page seeks instead of counting
    budget.rs     what this machine will let a sort or a filter hold
    natural.rs    ordering a digit run by what it counts, not by its first character
    edit.rs       the edit buffer: a sparse overlay + undo history
    writer.rs     splice edits back into the file, byte-preserving
    lake_db.rs    DuckLake access via DuckDB's ducklake extension
    rows.rs       row sets: which rows a :filter matched, and paging by index
    jsonl.rs      JSONL: find the keys, and build pages from the lines
  view.rs         the view language: parse and check :select/:hide/:filter/:sort/:expand/:reset
  complete.rs     completion for the `:` line, per position
  ui/
    table.rs      DataTable widget: renders DataFrame as a table
    browser.rs    Browser widget + cursor/scroll state for catalog lists
    cell.rs       CellWindow widget: one cell, in a window over the table
    json.rs       re-indent a JSON cell without rebuilding it
    markup.rs     re-indent an XML or HTML cell, likewise
    markdown.rs   name the parts of a markdown cell, changing nothing
    syntax.rs     what a run of a document is, and what colour that makes it
    help.rs       Help widget: the `?` key-binding overlay
    panel.rs      Panel widget: the candidate list above the status bar; the `zk` strip
    statusbar.rs  StatusBar widget: file/row/col position + help
    prompt.rs     the `:` and `/` line, with its caret
    theme.rs      every colour, in one place
```

The layers depend one way: `app.rs` asks `data::Store` for pages and edits and
hands them to the `ui` widgets. `Store` is the only door into `data/`, and the
widgets own no state that outlives a frame.

## Principles that run through all of it

These recur in every layer. Each README argues its own instances in place;
what is here is the rule they share, so that new code follows it without
having to rediscover it.

**Only what is on screen is materialised.** A file may be larger than memory,
so a page is parsed for the rows in view and nothing more, and what plv holds
follows what was changed rather than the size of the file — the row index keeps
a checkpoint every `STRIDE` rows rather than the rows, the edit overlay is
sparse, the writer streams in constant memory, a filter is a set of row
numbers. The one thing that must see the whole table is a held sort, and it is
bounded by what the machine has (`data/budget.rs`), counted in cells, and
refused past it. Bounds on reading are counted in **bytes**, never in rows,
because rows differ in width between files by more than an order of magnitude.

**The file's bytes are re-emitted, never re-serialised.** Parsing into a model
and printing it back loses key order, reformats numbers, drops duplicate keys,
rewrites escapes and changes quoting. So the writer splices only the edited
fields into the original bytes (a delimited field, or a JSON value at the byte
range it was read from), and the cell window's formatters emit only slices of
the value plus line breaks and spaces. A reader that quietly disagreed with the
writer about what is in the file would be worse than one that shows nothing.

**Ask before acting, so a refusal changes nothing.** Polars is lazy, so a bad
command left to fail in `collect()` fails later, as a planner error about nodes
the user never typed. The view language resolves names and checks literals
against the schema at the prompt instead, and underlines the word at fault. The
same rule makes `sort_blocked`, `edit_blocked`, `delete_blocked` and `pin_fits`
questions asked before the change is recorded, and puts the write's guards
before the rename.

**Source columns below `Store`, display positions above it.** `Store` counts
the file's own rows and columns; the app and the widgets count what is on show.
They cross only at named conversion points. Anything that must follow a column
through `:select` — hand-set widths, pins, the edit overlay — is keyed by source
column, which is why the schema only ever grows at its end (`:expand` appends,
and there is no un-expand) and why an edit on a hidden column is still written.

**Display state is not view state, and neither is edit history.** Which rows
and columns are being asked for is the `View`, written only through
`App::apply_view_command`. How they are drawn — widths, pins, the `zk` strip,
relative row numbers — is a fact about this screen and lives in `App`. `u` is
the edit buffer's undo and nothing else, because mixing display noise into the
data history would make it mean two things.

**Refuse rather than degrade silently, and say what was left out.** A fallback
that looks like the kindness often is not: a lazy sort of a table that does not
fit fails slowly rather than degrading, and a pin that leaves no room to scroll
is a table that silently stops answering `h` and `l`. So plv refuses and says
why. Where it keeps a partial answer, it says so — `(first n)` on a filter
that filled its set, `… n more` under a list, `none` for a search that found
nothing, hidden JSONL keys named when the file opens — because a list that
quietly stops reads as the whole list.

**One function answers, so two places cannot disagree.** Wherever two parts of
plv need the same answer — which line is the header (`loader::preamble`), where
the last column reaches the edge (`ui::col_offset_showing`), how wide a column
is drawn (`ui::drawn_width`), how a value wraps in its window
(`ui::cell::window`), when a field needs quotes (`writer::encode`), why an edit
is refused (`Store::edit_blocked`), which columns show
(`App::apply_view_command`) — one function gives it and both ask. Several of
these exist because two copies of the arithmetic once drifted apart.

**Measure, and keep the number.** The choices above were made against
measurements on real files — a 30GB CSV, a 2M-row CSV, an 842M-row census
parquet, a 1.14B-row lake table — and the numbers are recorded beside the
decisions they settled. A change that would move one is re-measured, not
assumed.
