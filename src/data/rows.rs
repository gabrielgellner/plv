//! Row sets: which rows of the file the viewer is looking at.
//!
//! A filter cannot be composed into the lazy frame the way a projection can.
//! Polars pushes a projection into the scan and lets the slice follow it down;
//! a filter stops the slice pushdown dead, so `filter(...).slice(offset, n)`
//! reads the *whole file* whatever offset it is asked for. Measured on a
//! 400k-row CSV: 18ms for the first page unfiltered against 101ms filtered,
//! and the gap grows with the file rather than staying put. Scrolling would
//! pay that on every keypress.
//!
//! So a filter is resolved once into the sorted set of source rows it matches —
//! exactly as `/` search already resolves a pattern — and paging becomes a
//! gather. The scan is the same shape as the search's and buys the same things:
//! progress, cancellation, a real row count, and row identity, which is what
//! lets the edit buffer survive a filter.

use polars::prelude::*;

use crate::view::{Filter, Op, Value};

/// The rows a filter matched, in file order, as they arrive.
#[derive(Debug, Default)]
pub struct RowSet {
    rows: Vec<usize>,
    complete: bool,
    /// How many rows this set may hold before it stops taking them.
    limit: usize,
    /// The limit was reached, so these are the first matches rather than all
    /// of them. Said out loud rather than left to look like the whole answer.
    truncated: bool,
}

impl RowSet {
    /// A set that will hold at most `limit` rows.
    ///
    /// One index per matching row is small until the matches are not: a filter
    /// keeping most of an 842M-row table is a 6.7GB `Vec`, arriving in batches
    /// so it grows quietly rather than failing at once. Past the limit the set
    /// keeps what it has and says it is partial, which leaves something usable
    /// on screen where refusing outright would not.
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            ..Self::default()
        }
    }

    /// Take a batch from the background scan. Batches arrive in file order.
    ///
    /// Returns whether the scan is still wanted; false once the set is full.
    pub fn extend(&mut self, batch: Vec<usize>) -> bool {
        let room = self.limit.saturating_sub(self.rows.len());
        if batch.len() > room {
            self.rows.extend(batch.into_iter().take(room));
            self.truncated = true;
            self.complete = true;
            return false;
        }
        self.rows.extend(batch);
        self.rows.len() < self.limit
    }

    /// Whether the set stopped short of every match.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn finish(&mut self) {
        self.complete = true;
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The source row shown at `display`.
    pub fn source(&self, display: usize) -> Option<usize> {
        self.rows.get(display).copied()
    }

    /// Where a source row appears, if it survived the filter. The rows are in
    /// file order, so this is a binary search.
    pub fn display(&self, source: usize) -> Option<usize> {
        self.rows.binary_search(&source).ok()
    }

    /// The source rows for one page.
    pub fn page(&self, offset: usize, height: usize) -> &[usize] {
        let start = offset.min(self.rows.len());
        let end = start.saturating_add(height).min(self.rows.len());
        &self.rows[start..end]
    }
}

/// Turn a parsed filter into the expression the scan runs.
///
/// Returns `None` if a column index no longer exists, which can only happen if
/// the file was reopened under a different schema.
pub fn predicate(filter: &Filter, schema: &Schema) -> Option<Expr> {
    filter
        .conditions
        .iter()
        .map(|condition| {
            let (name, _) = schema.get_at_index(condition.column)?;
            Some(one(col(name.as_str()), condition.op, &condition.value))
        })
        .collect::<Option<Vec<Expr>>>()?
        .into_iter()
        .reduce(Expr::and)
}

fn one(column: Expr, op: Op, value: &Value) -> Expr {
    match value {
        // An empty literal asks for the cells with nothing in them, matching
        // how plv renders and writes an empty field everywhere else.
        Value::Empty => match op {
            Op::Ne | Op::NotMatches => column.is_not_null(),
            _ => column.is_null(),
        },
        // Regex reads the column as text, exactly as `/` search does, so it
        // works on a numeric column too.
        Value::Pattern(pattern) => {
            let matches = column
                .cast(DataType::String)
                .str()
                .contains(lit(pattern.as_str()), false);
            if op == Op::NotMatches {
                matches.not()
            } else {
                matches
            }
        }
        Value::Number(n) => compare(column, op, lit(*n)),
        Value::Bool(b) => compare(column, op, lit(*b)),
        Value::Text(s) => compare(column, op, lit(s.as_str())),
    }
}

fn compare(column: Expr, op: Op, value: Expr) -> Expr {
    match op {
        Op::Eq => column.eq(value),
        Op::Ne => column.neq(value),
        Op::Lt => column.lt(value),
        Op::Le => column.lt_eq(value),
        Op::Gt => column.gt(value),
        Op::Ge => column.gt_eq(value),
        // Handled before this point; a regex never reaches here.
        Op::Matches | Op::NotMatches => column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(rows: &[usize]) -> RowSet {
        let mut set = RowSet::new(usize::MAX);
        set.extend(rows.to_vec());
        set.finish();
        set
    }

    #[test]
    fn a_row_set_maps_both_ways() {
        let set = set(&[3, 7, 11]);
        assert_eq!(set.source(0), Some(3));
        assert_eq!(set.source(2), Some(11));
        assert_eq!(set.source(3), None);

        assert_eq!(set.display(7), Some(1));
        assert_eq!(set.display(8), None, "row 8 did not survive the filter");
    }

    #[test]
    fn a_page_is_clamped_to_what_has_arrived() {
        let set = set(&[1, 2, 3, 4, 5]);
        assert_eq!(set.page(0, 3), [1, 2, 3]);
        assert_eq!(set.page(3, 10), [4, 5], "past the end, not out of bounds");
        assert!(set.page(9, 3).is_empty());
    }

    #[test]
    fn a_full_set_stops_the_scan_and_owns_up() {
        let mut set = RowSet::new(4);
        assert!(set.extend(vec![1, 2]), "room for more");
        assert!(
            !set.extend(vec![3, 4, 5, 6]),
            "the scan is no longer wanted"
        );

        assert_eq!(set.len(), 4, "kept what fits");
        assert!(
            set.is_truncated(),
            "and does not pretend that is all of them"
        );
        assert!(set.is_complete(), "nothing further is coming");
        assert_eq!(set.source(3), Some(4));
        assert_eq!(set.source(4), None);
    }

    #[test]
    fn a_set_that_exactly_fills_its_limit_is_not_called_truncated() {
        let mut set = RowSet::new(3);
        assert!(!set.extend(vec![1, 2, 3]), "full, so stop asking");
        assert!(!set.is_truncated(), "every match fitted");
    }

    #[test]
    fn batches_accumulate_until_the_scan_finishes() {
        let mut set = RowSet::new(usize::MAX);
        assert!(!set.is_complete());
        set.extend(vec![1, 4]);
        assert_eq!(set.len(), 2);
        set.extend(vec![9]);
        assert_eq!(set.len(), 3);
        assert_eq!(set.display(9), Some(2), "still sorted across batches");
        set.finish();
        assert!(set.is_complete());
    }
}
