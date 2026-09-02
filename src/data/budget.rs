//! How much memory plv is willing to spend holding data.
//!
//! Two things scale with the file rather than with the viewport: a held sort,
//! which is the whole table in memory, and a filter's row set, which is one
//! index per matching row. Both were bounded by constants picked on a 24GB
//! machine, which is no use on an 8GB one — the point of a bound is that it
//! relates to what is actually there.
//!
//! **Cells, not bytes on disk.** Measured over three shapes of real data, a
//! held sort costs 35 bytes per cell for all-integer columns, 82 for
//! all-string, and 46 for the mixed census table. Against the source file's
//! size the same three are 3.5×, 4.8× and 8.8× — a wider spread, and file size
//! is no easier to come by. So the estimate is per cell, calibrated to the
//! worst of them with room to spare.

use std::sync::OnceLock;

use sysinfo::{MemoryRefreshKind, RefreshKind, System};

/// What a cell of a held table costs, near enough. The worst shape measured
/// was 82 bytes; this leaves room for one worse.
const BYTES_PER_CELL: u64 = 128;

/// Share of the machine a held sort may take. It is a deliberate keystroke on
/// a tool whose job is the table, so this is generous — but a quarter is
/// still a quarter, and the refusal that follows says so.
const SORT_SHARE: f64 = 0.25;

/// Share for a filter's row set. Smaller, because unlike a sort it grows
/// while you watch and the rows it points at are still read lazily.
const FILTER_SHARE: f64 = 0.05;

/// Total physical memory, asked once.
fn total_memory() -> u64 {
    static TOTAL: OnceLock<u64> = OnceLock::new();
    *TOTAL.get_or_init(|| {
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        // A machine that will not say gets treated as a small one.
        match system.total_memory() {
            0 => 4 << 30,
            bytes => bytes,
        }
    })
}

/// Cells a held sort may cover.
pub fn sort_cells() -> usize {
    cells(SORT_SHARE)
}

/// Rows a filter's set may hold, at one index each.
pub fn filter_rows() -> usize {
    let bytes = (total_memory() as f64 * FILTER_SHARE) as u64;
    (bytes / size_of::<usize>() as u64).max(1_000_000) as usize
}

fn cells(share: f64) -> usize {
    let bytes = (total_memory() as f64 * share) as u64;
    // A floor, so a small machine can still sort a small file.
    (bytes / BYTES_PER_CELL).max(1_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bounds_follow_the_machine_and_stay_sane() {
        let total = total_memory();
        assert!(total > 0, "some memory was reported");

        let sort = sort_cells();
        let filter = filter_rows();
        assert!(
            sort >= 1_000_000,
            "a small machine can still sort something"
        );
        assert!(filter >= 1_000_000);

        // Neither may promise more than the machine has.
        assert!((sort as u64).saturating_mul(BYTES_PER_CELL) <= total);
        assert!((filter as u64).saturating_mul(8) <= total);
        assert!(
            sort > filter / 100,
            "the two are the same order of magnitude"
        );
    }

    /// The numbers this is calibrated against, so a later change to
    /// `BYTES_PER_CELL` has to argue with them.
    #[test]
    fn the_estimate_covers_what_was_measured() {
        // (cells, peak bytes) from sorting real files.
        let measured = [
            (10_000_000u64, 349_323_264u64), // all integers, 35 B/cell
            (10_000_000, 815_759_360),       // all strings,  82 B/cell
            (50_000_000, 2_321_088_512),     // census mix,   46 B/cell
        ];
        for (cells, peak) in measured {
            assert!(
                cells * BYTES_PER_CELL >= peak,
                "{cells} cells estimated at {} but cost {peak}",
                cells * BYTES_PER_CELL
            );
        }
    }
}
