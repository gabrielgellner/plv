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
| `j` / `k` | Scroll down / up |
| `Ctrl+d` / `Ctrl+u` | Half page down / up |
| `g` / `G` | Jump to top / bottom |
| `h` / `l` | Scroll columns left / right |
| `H` | Jump to first column |
| `q` | Quit |

## Install

```
cargo install --path .
```

## Build

```
cargo build --release
```
