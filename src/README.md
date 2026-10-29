# src — plv's source

## System Overview

plv is three layers, and the dependencies run one way. The **app layer** (the
files in this directory) holds the session: the event loop, the key bindings,
the cursor, the screens, and the small state machines beside them — the `:`
view language, completion, the column picker, `/` search, the clipboard and the
DuckLake browse state. It asks the **data layer**
([`data/README.md`](./data/README.md)) for pages and edits, and hands what it
gets to the **UI layer** ([`ui/README.md`](./ui/README.md)) to draw. The data
layer knows nothing of the screen and the UI layer owns no state that outlives a
frame; the app layer is the only place both are in view.

`main.rs` is a thin binary over the `plv` library (`lib.rs`), which is also
what [`../examples/`](../examples/README.md) links against.

## Subdirectories & System Boundaries

- `data/`: [`./data/README.md`](./data/README.md) — opening, indexing,
  paging, filtering, sorting, editing and writing files, and DuckLake access.
  Everything below `Store` counts **source** rows and columns.
- `ui/`: [`./ui/README.md`](./ui/README.md) — the ratatui widgets, and the
  layout arithmetic the app layer has to agree with.

## Local Interface & Invariants

- `main.rs` — clap `Args { file: Option<PathBuf> }`; initialises and restores
  the terminal, enables bracketed paste, runs `App`.
- `lib.rs` — `pub mod` for every module, so the binary and the examples share
  one crate.
- `app.rs` — `App::new(Option<PathBuf>)` and `App::run(&mut DefaultTerminal)`
  are the whole public surface; everything else (`AppMode`, `Screen`, the key
  handlers, `apply_view_command`, `paste_block`, `help_sections`,
  `display_pins`, `MAX_BLOCK`) is private to it.
- `view.rs` — `parse(&str, &Schema) -> Result<Command, ParseError>`;
  `Command { Select, Hide, Expand, Filter, Sort, Reset }`, `Slot`, `Filter`,
  `Condition`, `Op`, `Value`; `ParseError { message, span }` with `caret`;
  `View { select, filter, sort }` with `apply`, `columns`, `describe`,
  `is_empty`. Pure.
- `complete.rs` — `complete(&str, &Schema) -> Option<Completion>`,
  `Completion { head, word, options }`, `VIEW_VERBS`, `FILE_VERBS`. Pure.
- `picker.rs` — `Picker`, a working copy of which columns show and which are
  pinned, with `toggle_shown`, `show_all`, `show_only_cursor`, `toggle_pinned`,
  `selection`, `pins`. Pure.
- `search.rs` — `SearchQuery`, `SearchStatus`, `SearchState` (the matching
  rows, the current one, `next_from`/`prev_from`). Pure; the scan that fills it
  is `Store::search_async`.
- `clipboard.rs` — `tsv`, `copy`, `parse`, `MAX_BYTES`.
- `lake.rs` — `Lake` (the browse state over a `LakeDb`), `Level { Tables,
  Partitions { table }, Snapshots }`, `Scope { table, partition }`.

Invariants:

- Every change to which columns show, in what order, filtered and sorted how,
  goes through `App::apply_view_command` — `:select`, `:hide`, `-`, the picker
  and `:expand` alike — so there is one answer to what the view is.
- A sort is asked whether it is allowed (`Store::sort_blocked`) before it is
  recorded, and an edit or a delete before it is made (`edit_blocked`,
  `delete_blocked`); a refusal leaves everything as it was.
- Column widths and pins (`App::widths`, `App::pinned`) are keyed by source
  column and are display state: they are not in the `View`, and `u` does not
  take them back.
- `view.rs` resolves names and checks literals against the schema at the
  prompt; nothing past it sees an unchecked command.
- `q` and `:q` refuse while edits are unwritten.

## Global Side Effects

- The terminal: raw mode and the alternate screen (`main.rs`); the event loop
  blocks on input, and polls every 50ms while background work is in flight.
- stdout: the OSC 52 sequence on `y` (`clipboard::copy`, only when stdout is a
  terminal and the text fits `MAX_BYTES`), and the bracketed-paste switches.
- Files: `:w`, `:wq` and `:x` write through `Store::save` — see
  [`data/README.md`](./data/README.md#editing).
- Threads: sorts, filters and searches run on threads the store spawns, and
  report over `mpsc`; dropping the receiver cancels them.
- DuckDB: `Lake::new` and `Lake::load_partitions` query the lake; selecting a
  snapshot re-attaches it.

## Design

### The app layer (`app.rs`)

- `App` owns the session: `Option<Store>` (or, for a lake, `Option<Lake>` and the `Screen` it is on), the cursor and `col_offset`, the input mode (`AppMode`: `Normal`, `Search`, `Edit`, `Command`, `Picker`), the selection mode and visual anchor, the edit register, the `:` and `/` buffers and the completion state, the receivers for background sort, filter and search work, and the display state — `widths`, `pinned`, `relative_rows`, the `zk` strip, the picker and the `K` window (`Inspect`). A file that failed to open leaves its reason in `error: Option<String>`.
- `run()`: asks `lake_db::detect` whether the path is a lake. A lake opens a `LakeDb` and a `Lake` on `Screen::Browser`; anything else opens `Store::open_file` sized to the terminal, and what the store has to say about the file (`Store::notes`, such as the JSONL keys it hid) goes in the message line. Then it enters the event loop.
- `draw()`: resizes the store to the rows left once the panel has taken its own, then renders the browser or the `DataTable` + `StatusBar`, with the panel, the prompt, and whichever of the picker, the cell window and the help overlay is up (or the usage text if no file was given).
- Vim key bindings: `j/k` (±1 row), `Ctrl+d`/`Ctrl+u` (half a screen: the view and the cursor move together, as in vim, rather than the cursor walking to the edge first), `gg/G` (top/bottom), `{n}gg`/`{n}G` (row n), `zz`/`zt`/`zb` (scroll the cursor row to the middle/top/bottom of the viewport), `h/l` and `{n}h`/`{n}l` (columns, counted like `j`/`k`), `0`/`$` (scroll so the first/last column sits at its edge), `H` (leftmost column), `zp`/`z|` (pin the cursor column at the left edge / unpin every column), `-` (hide the cursor column), `C` (the column picker), `K` (the cursor cell in a window), `zk` (the cursor cell in the strip), `q` (quit). Arrow keys mirror `j/k/h/l`. Besides these: `Ctrl+f`/`Ctrl+b` and `PageDown`/`PageUp` (a screen), `Home`/`End`, `Q` (quit, as `q`), `/` `n` `N` (search), `s` (sort by the cursor column), `Tab` (cycle the selection mode, row → column → cell), `v` (visual selection), `#` (absolute or relative row numbers), `z>`/`z<`/`z_`/`z=` (column widths), `?` (help), and the editing keys below. `Esc` takes off one layer at a time: the strip, then the visual selection, then the search and the sort. The `?` overlay (`App::help_sections`) is the complete list.
- Horizontal movement stops where the last column reaches the right edge, in every selection mode: scrolling past it would pad the view with empty space instead of data. `ui::col_offset_showing()` answers that question, and lives beside the renderer that has to agree with it — the app layer used to keep its own copy of the arithmetic and the two drifted apart.

### Editing keys

The edit buffer, the writer and why editing works the way it does are in [`data/README.md`](./data/README.md#editing); these are the keys over it.

**Keys.** `i`/`a`/`c` open a cell (caret at the front, at the end, or empty), `x`
clears it, `u`/`Ctrl+r` undo and redo, `y`/`p` yank and paste through an internal
register. `v` starts a visual selection whose shape follows
the `Tab` mode — whole rows, whole columns, or a rectangle. Over a selection `c`
replaces every cell while `i` and `a` prepend and append to what is already
there, following vim's blockwise `I` and `A`; those two read the block first, so
they see rows below the viewport. Block operators are capped at `MAX_BLOCK` cells,
because a column-mode selection covers every row in the file.

`:` opens an ex line: `:w`, `:w!`, `:w path`, `:q`, `:q!`, `:wq`, `:x`. Bare `q`
and `:q` refuse while edits are unwritten. The status bar carries a `[+n]` count
and edited cells render in red.

### The clipboard (`clipboard.rs`)

`y` puts the block on the **system clipboard** as TSV, and the terminal's own
paste key puts one back — two directions, no dependency. A clipboard crate
would mean linking platform windowing libraries and, on X11 and Wayland, a
thread to keep the copied data alive after plv exits; it can also fail to build
on a headless box, which is a bad trade for a viewer.

Out is **OSC 52**, an escape sequence asking the terminal to set its clipboard,
so it works over SSH where a native clipboard would be reaching for the wrong
machine's. In is **bracketed paste** — a default crossterm feature, so
`Event::Paste` arrives without asking for anything — which is the read half OSC
52 does not reliably have, since terminals disable reading it back for the
obvious reason. The cost is that the keys are asymmetric: `y` copies, but
pasting in is the terminal's key rather than `p`.

TSV because that is what a spreadsheet puts on the clipboard for a range and
what it expects back, and fields are quoted by `writer::encode` — the same
function the file writer uses, because the two have to agree about when a value
needs quotes or a cell holding a tab arrives elsewhere as two. `clipboard::parse`
is its inverse and a test round-trips a block through both.

A paste goes through `App::paste_block`, the same path `p` takes, so the
clipping at the last row and column and the count of what fell off the edge have
one answer whichever side the block came from — and it leaves the register
alone, since a paste is not a yank. A paste arriving while a `:` line, a `/`
pattern or a cell is being typed goes into *that* instead, with newlines
(and tabs) flattened to spaces: a line holds one line. Past `clipboard::MAX_BYTES` a yank
is kept in the register and not sent, because a terminal that truncates the
sequence leaves half a block on the clipboard with nothing about it looking
wrong.

### The view language (`view.rs`)

`:select`, `:hide`, `:filter` and `:sort` shape what the viewer shows, and
`:expand` (JSONL only) changes what columns there are to show. The module parses
and checks; nothing in it touches a `LazyFrame`.

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
The sort worked this way before the view language did, and `s` still writes
the slot `:sort` replaces: `Store::begin_sort` adds the cursor column to
`View::sort`, or flips its direction if it is already there.

Evaluation order is fixed independently of the order commands were typed:
**filter → sort → select**, as in SQL, so a filter or sort can name a column
that is not on show.

Grammar, deliberately closed: `~` and `!~` are regex and read any column as text
(as `/` search does); other comparisons require a literal matching the column's
type; an empty literal `""` means the cells with nothing in them, matching how
plv renders and writes empty fields elsewhere. A date, datetime or time column
takes `2024-01-01`, `"2024-01-01 12:00"` or `09:30`, parsed by plv into a value
of the column's own type rather than cast by Polars, whose string casts accept
only some of the forms it prints and warn about it on stderr, over the TUI. A
zoned column reads the literal as wall time in its zone, since that is what the
table shows, and a time the zone skipped or repeated is refused rather than
guessed. A list, struct, duration or bytes column has no literal at all and
says so: only `~` and `""` reach it. "Any column as text" is `rows::as_text`,
which casts where Polars has a cast and otherwise draws each value the way the
table does — one column without a cast used to make `/` find nothing at all. Conditions join with `and` only —
no `or` and no parentheses, because precedence cannot be introduced later
without changing what already-written commands mean.

How a filter and a sort are then *run* — the row set, the held sort, natural ordering, the memory budget — is the data layer's, in [`data/README.md`](./data/README.md#filters-and-sorts).

### Hiding a column, and the column picker (`picker.rs`)

`-` hides the cursor column, and is sugar for typing `:hide <name>` —
deliberately nothing more. It goes through `App::apply_view_command`, the same
path a typed line takes, so there is one answer to which columns show rather
than two that can disagree; a separate hidden-set beside `widths` and `pinned`
would be the drift these documents keep warning about, because *whether* a column is
on show is exactly what the `View` is for. `:hide` reads the current list and
filters it, so `-` twice narrows twice where a second `:select` would replace
the first, and hiding the last column is refused by `View::apply` rather than by
the key. The cursor is left where the column was, on whatever moved into that
position — `dd`'s rule, and it falls out of the clamp in `after_view_change`.
There is no key that un-hides one column, since naming it is the only way to say
which, so the message says `:reset select` at the moment the user might want it.
In row mode `-` **adopts** a column cursor through `App::adopt_column_cursor`,
the leftmost visible column, exactly as the edit keys and `s` do: row mode is
what plv opens in, and a key that silently does nothing there is
indistinguishable from one that does not exist. `-` and `s` make no exception
for a row-shaped selection where an edit does — an edit over whole rows is
meaningful and must not be reshaped under the operator about to run, while these
two clear the selection anyway (hiding renumbers the columns after it, and
sorting reorders every row), so there is nothing to protect. `s` adopts *after*
`sort_blocked` is asked, so a refused sort leaves the selection mode as it was
along with everything else.

`C` opens the **column picker** (`src/picker.rs`), the other direction: a list of
every column with a tick for shown and one for pinned, `-` and `p` to toggle,
`Enter` to apply and `Esc` to leave. `a` shows every column and `A` hides all but
the one under the cursor — picking four columns out of two hundred means starting
from none, and unticking 196 by hand is not a thing anyone will do. Two keys and
not one toggle, because the state a toggle would read is a hundred rows long and
mostly off screen, and a key whose direction depends on what you cannot see is a
guess; `A` keeps the cursor's column because that is where the handful gets built
up from, and lands on exactly one, which is the fewest a view may have. `-` and
not Space, so the key that takes a
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

### Completion (`complete.rs`)

Tab on the `:` line completes, deliberately **not** fuzzily: this is a small
closed vocabulary — a few verbs and the file's own column names — that the user
is trying to type exactly, where a prefix match is predictable and a fuzzy one
is a guess. Fuzzy matching belongs on `/`, over the data.

What is offered depends on where in the line the cursor is: verbs at the front,
column names after `:select`/`:hide`/`:expand`/`:sort`, slot names after `:reset`, and
inside a `:filter` the positions cycle through column, operator, value, `and` —
a value being the file's own data, which is not a vocabulary to complete
against. A test asserts every verb completion offers is one `view::parse`
accepts, so the two cannot drift.

Candidates are matched against the bare column name and offered in the form that
has to be typed, so `rel` finds `release date` and writes `"release date"`.
Tab extends the line as far as the candidates agree and never shortens it, then
steps through them; Shift+Tab steps back.

How the candidates are drawn is in [`ui/README.md`](./ui/README.md#the-completion-panel-panelrs).

### The DuckLake browser (`lake.rs`)

Pointing plv at a `.ducklake` file — or a directory containing one — opens the
**catalog browser** instead of the table viewer.

**Screens.** `App` has two: `Screen::Browser` (the lake) and `Screen::Viewer`
(the existing `DataTable`). `lake.rs` holds the browse state: `Level::Tables`
lists the lake's tables, `Level::Partitions { table }` lists that table's
partition values, `Level::Snapshots` lists snapshots, and `Scope` records what
the viewer is scanning.

Browser keys: `j/k` move, `g/G` top/bottom, `l`/`f` descend into the partition
list, `T` list snapshots, `h`/`Esc` back, `Enter` open the selection, `a` open
the whole table from within the partition list. `q` quits, and `Ctrl+d`/`Ctrl+u`, the arrow keys
and `Home`/`End` move as they do in the table. In the viewer, `f` returns to
the partitions, `b` to the browser, `T` to the snapshot picker.

**Time travel** is an ATTACH option (`SNAPSHOT_VERSION`), so selecting a
snapshot re-opens the lake rather than rewriting every query. If the table the
viewer was showing still exists at the target snapshot it is reopened by *name*,
so the same data can be compared across snapshots.

How the lake is read is in [`data/README.md`](./data/README.md#ducklake-lake_dbrs).

### Polars API notes

The features plv needs are not in the default set and are named explicitly —
`lazy` and `parquet` for the readers, `strings` and `regex` for what `/` and
`:filter ~` ask of a column, and `dtype-full` with `timezones` because a parquet
file brings its own types. Polars only knows the types it was compiled with: a
`UInt8` column was an error at open and a `Decimal` one a panic, and a zoned
datetime panics as it is drawn without `timezones`:
```toml
polars = { version = "0.55.2", features = ["lazy", "parquet", "strings", "regex", "dtype-full", "timezones"] }
```

Key API differences from older polars:
- `LazyFrame::schema()` → `LazyFrame::collect_schema()` (takes `&mut self`)
- `DataFrame::get_columns()` → `DataFrame::columns()` (returns `&[Column]`)
- `count()` expression → `len()` expression
- `LazyCsvReader::new(path)` and `LazyFrame::scan_parquet(path, args)` both take `PlRefPath`, converted via `PlRefPath::try_from_path(&Path)`

## System Verification

- Run: `cargo test`, and `cargo clippy`, which must be clean before committing
  (see [`../CONTRIBUTING.md`](../CONTRIBUTING.md)).
