//! Scratch harness: dump what the catalog reader sees for a lake.
use plv::data::catalog::{self, Catalog};

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).expect("usage: lakedump <path>");
    let path = catalog::detect(std::path::Path::new(&arg)).expect("no .ducklake found");
    let cat = Catalog::open(&path)?;
    println!("catalog:   {}", cat.path.display());
    println!("data_root: {}", cat.data_root.display());
    println!("snapshot:  {} of {}", cat.snapshot, cat.snapshots.len());
    for s in &cat.snapshots {
        println!("  #{} {} schema_v{} {}", s.id, s.time, s.schema_version, s.changes);
    }
    for t in &cat.tables {
        println!(
            "\n== {} : {} rows, {}, {} files, part[{}], inlined {}",
            t.qualified_name(),
            catalog::human_count(t.record_count),
            catalog::human_bytes(t.file_size),
            t.files.len(),
            t.partition_cols.join(","),
            t.inlined_rows
        );
        for c in &t.columns {
            println!("   col {} {} null={}", c.name, c.ty, c.nullable);
        }
        for f in t.files.iter().take(4) {
            println!(
                "   file {} {} rows={} exists={}",
                f.id, f.label(), catalog::human_count(f.record_count), f.path.exists()
            );
        }
    }
    // Prove a scan works: read the head of the smallest file of the first table.
    if let Some(t) = cat.tables.first() {
        let smallest = t.files.iter().min_by_key(|f| f.record_count).unwrap();
        println!("\nscanning {} ...", smallest.label());
        let lf = cat.scan_files(t, std::slice::from_ref(smallest))?;
        let df = lf.slice(0, 3u32).collect()?;
        println!("{df}");
    }
    Ok(())
}
