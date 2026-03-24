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
| `H` | Jump to first column |
| `zz` / `zt` / `zb` | Center / top / bottom cursor in view |
| `q` | Quit |

## Search

| Key | Action |
|-----|--------|
| `/` | Open search prompt |
| `n` | Jump to next match |
| `N` | Jump to previous match |
| `Esc` | Clear active search |

Type a regex pattern after `/` and press `Enter` to search across all rows and columns. Matches are highlighted in the table and the status bar shows the current position (`/pattern [2/15]`). Press `Esc` at the prompt to cancel without searching.

## Install

```
cargo install --path .
```

## Build

```
cargo build --release
```
