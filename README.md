# plv

A terminal viewer for CSV and Parquet files, inspired by [csvlens](https://github.com/YS-L/csvlens). Built with [Polars](https://pola.rs/) and [ratatui](https://ratatui.rs/).

- Supports CSV and Parquet
- Larger-than-memory files via Polars lazy evaluation
- Vim-style navigation

## Usage

```
plv <file.csv|file.parquet>
```

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

## Install

```
cargo install --path .
```

## Build

```
cargo build --release
```
