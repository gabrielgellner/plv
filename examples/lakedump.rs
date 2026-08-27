//! Scratch harness: dump what plv sees in a lake, through the DuckLake reader.
use plv::data::lake_db::{self, LakeDb};

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).expect("usage: lakedump <path> [snapshot]");
    let snapshot = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let path = lake_db::detect(std::path::Path::new(&arg)).expect("no .ducklake found");

    let db = LakeDb::open(&path, snapshot)?;
    println!("lake:     {}", db.path.display());
    println!("snapshot: {}", db.current_snapshot()?);
    for s in db.snapshots()? {
        println!(
            "  #{} {} schema_v{} {}",
            s.id,
            s.short_time(),
            s.schema_version,
            s.commit_message.unwrap_or(s.changes)
        );
    }

    for table in db.tables()? {
        println!(
            "\n== {} : {} rows, {}, {} files, {} deletes, part[{}]",
            table.qualified_name(),
            lake_db::human_count(table.rows),
            lake_db::human_bytes(table.file_size),
            table.file_count,
            table.delete_file_count,
            table.partition_cols.join(",")
        );
        for p in db.partitions(&table)?.iter().take(4) {
            println!("   {} — {} rows", p.label(), lake_db::human_count(p.rows));
        }
        let source = db.source(&table, None);
        println!("   count = {}", lake_db::human_count(db.count(&source)? as u64));
        println!("{}", db.page(&source, &[], 0, 3)?);
    }
    Ok(())
}
