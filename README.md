# plv

A terminal viewer for CSV, Parquet and [DuckLake](https://ducklake.select/) data, inspired by [csvlens](https://github.com/YS-L/csvlens). Built with [Polars](https://pola.rs/) and [ratatui](https://ratatui.rs/).

- Supports CSV, Parquet, and DuckLake lakes
- Larger-than-memory files via Polars lazy evaluation
- Browse a lake's tables, partition files and snapshots — including time travel
- Vim-style navigation

## Usage

```
plv <file.csv>
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
| `Ctrl+d` / `Ctrl+u` | Half page down / up |
| `g` / `Home` | Jump to first row |
| `G` / `End` | Jump to last row |
| `{n}G` | Jump to row n |
| `h` / `←` | Scroll columns left |
| `l` / `→` | Scroll columns right |
| `H` | Jump to first column (all modes) |
| `0` | Jump to first column (column/cell mode) |
| `$` | Jump to last column (column/cell mode) |
| `Tab` | Cycle selection mode: row → column → cell |
| `s` | Sort by cursor column (column/cell mode); toggles asc ↔ desc; add more columns for multi-sort |
| `zz` / `zt` / `zb` | Center / top / bottom cursor in view |
| `?` | Show key bindings for the current screen |
| `q` | Quit |

## Selection modes

Press `Tab` to cycle through three selection modes:

- **Row** (default) — entire cursor row is highlighted; search covers all columns
- **Column** — the current column is highlighted; search covers only that column; press `s` to sort, `Esc` to clear all sorts
- **Cell** — only the cursor cell is highlighted; search covers only the current column; press `s` to sort, `Esc` to clear all sorts

## Search

| Key | Action |
|-----|--------|
| `/` | Open search prompt |
| `n` | Jump to next match |
| `N` | Jump to previous match |
| `Esc` | Clear active search |

Type a regex pattern after `/` and press `Enter`. In Row mode, search covers all columns. In Column or Cell mode, search is scoped to the selected column. Matches are highlighted in the table and the status bar shows progress (`/pattern [2/15]`).

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
**data files**, so you can scope the view to a single partition instead of
scanning the whole table — which matters when one partition is most of the data:

```
│GEO_LEVEL=DisseminationArea            842,209,475   800.2 MB   73.8%  │
│GEO_LEVEL=Country                      14,545        73.0 KB    0.0%   │
```

Partition columns are reconstructed from the catalog, so `GEO_LEVEL` appears as a
real column even though it only exists in the file path.

### Keys

| Key | Action |
|-----|--------|
| `Enter` | Open the selected table, file, or snapshot |
| `l` / `f` | Show the selected table's data files |
| `a` | Open the whole table (from the file list) |
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

### Notes and limitations

- The catalog is opened **read-only**. If another process holds it open for
  writing, plv says so rather than waiting.
- DuckDB is used only to read catalog metadata; Polars does all the scanning.
- DuckLake can keep recent rows **inlined** in the catalog database rather than
  in Parquet. plv reports the count in the browser and warns when you open such
  a table, but does not yet merge those rows into the view — so a table with
  inlined rows shows slightly fewer rows than a DuckDB query would.

## Install

```
cargo install --path .
```

## Build

```
cargo build --release
```
