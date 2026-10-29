# examples

## Overview

`lakedump.rs` is a scratch harness: it prints what plv sees in a DuckLake
catalog, through the same reader the browser uses
([`src/data/lake_db.rs`](../src/data/README.md#ducklake-lake_dbrs)). When a
lake looks wrong in the browser, this says whether the catalog reader or the
screen is the one getting it wrong, with no TUI in the way.

## Interface

- `cargo run --example lakedump -- <path> [snapshot]` — `<path>` is a
  `.ducklake` file or a directory containing one (resolved by
  `lake_db::detect`); `snapshot` is an optional snapshot id to attach at.
- Prints the lake path, the current snapshot and every snapshot with its schema
  version and commit message; then, per table, its row count, size, file and
  delete-file counts, partition columns, the first four partitions, the
  `count(*)` the viewer would use, and the first three rows of its first page.

## Invariants

- It uses only the public `plv::data::lake_db` API (`LakeDb::open`,
  `snapshots`, `tables`, `partitions`, `source`, `count`, `page`), so what it
  prints is what the browser is given. It must not grow a reader of its own.

## Side Effects

- Attaches the lake read-only through DuckDB (see `lake_db.rs`); writes only to
  stdout.

## Verification

- Run: `cargo build --examples` (it has no tests); `cargo test` for the reader
  it calls.
