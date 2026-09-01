//! Pending cell edits: the buffer between a keystroke and `:w`.
//!
//! plv reads lazily — only the visible rows are ever materialized — so there is
//! no `DataFrame` to mutate. Edits live here instead, as a sparse overlay keyed
//! by position in the *source* file, stamped onto each page as it is rendered
//! and spliced into the file on write. Memory is proportional to the number of
//! edits, not to the size of the file.
//!
//! Values are `String` because the file is text: the types plv displays come
//! from Polars' inference over the bytes on disk, not from the bytes
//! themselves. Whether a new value still parses as its column's type is a
//! warning for the caller to raise, not a rule this layer enforces.

use std::collections::BTreeMap;

/// A cell's position in the source file: the 0-based data row — the header is
/// not counted — and the field index.
pub type Cell = (usize, usize);

/// One undoable step, recorded as the values that were there before it. `None`
/// means the cell held no pending edit, so undoing it removes the entry.
///
/// A step is a `Vec` rather than a single cell because a fill over a visual
/// range is one edit as far as the user is concerned, and `u` should take all
/// of it back at once.
type Change = Vec<(Cell, Option<String>)>;

/// Pending edits, grouped by row.
///
/// Grouped because both readers want them that way: the renderer asks for the
/// rows in the current page, and the writer walks records in file order.
#[derive(Default)]
pub struct Overlay {
    rows: BTreeMap<usize, BTreeMap<usize, String>>,
    len: usize,
    undo: Vec<Change>,
    redo: Vec<Change>,
}

impl Overlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cells carrying a pending edit.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, (row, col): Cell) -> Option<&str> {
        self.rows.get(&row)?.get(&col).map(String::as_str)
    }

    /// Pending edits for one row, keyed by field index.
    pub fn row(&self, row: usize) -> Option<&BTreeMap<usize, String>> {
        self.rows.get(&row)
    }

    /// Every edited row in ascending order.
    pub fn rows(&self) -> impl Iterator<Item = (usize, &BTreeMap<usize, String>)> {
        self.rows.iter().map(|(&row, cells)| (row, cells))
    }

    /// Apply `edits` as a single undoable change. An empty batch is ignored, so
    /// it never leaves a step on the stack that `u` would appear to skip.
    pub fn set<I: IntoIterator<Item = (Cell, String)>>(&mut self, edits: I) {
        let change: Change = edits
            .into_iter()
            .map(|(cell, value)| (cell, self.insert(cell, value)))
            .collect();
        if change.is_empty() {
            return;
        }
        self.undo.push(change);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Take back the last change. Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(change) = self.undo.pop() else {
            return false;
        };
        let inverse = self.revert(change);
        self.redo.push(inverse);
        true
    }

    /// Re-apply the last undone change.
    pub fn redo(&mut self) -> bool {
        let Some(change) = self.redo.pop() else {
            return false;
        };
        let inverse = self.revert(change);
        self.undo.push(inverse);
        true
    }

    /// Drop every pending edit *and* the history. Called after a successful
    /// write: the file now holds these values, so undoing past it would
    /// silently reintroduce changes the user believes they saved.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.len = 0;
        self.undo.clear();
        self.redo.clear();
    }

    /// Restore the values in `change`, returning the change that would restore
    /// what was there before this call.
    fn revert(&mut self, change: Change) -> Change {
        change
            .into_iter()
            .map(|(cell, previous)| {
                let current = match previous {
                    Some(value) => self.insert(cell, value),
                    None => self.remove(cell),
                };
                (cell, current)
            })
            .collect()
    }

    fn insert(&mut self, (row, col): Cell, value: String) -> Option<String> {
        let previous = self.rows.entry(row).or_default().insert(col, value);
        if previous.is_none() {
            self.len += 1;
        }
        previous
    }

    fn remove(&mut self, (row, col): Cell) -> Option<String> {
        let cells = self.rows.get_mut(&row)?;
        let previous = cells.remove(&col);
        if previous.is_some() {
            self.len -= 1;
        }
        if cells.is_empty() {
            self.rows.remove(&row);
        }
        previous
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set1(overlay: &mut Overlay, cell: Cell, value: &str) {
        overlay.set([(cell, value.to_string())]);
    }

    #[test]
    fn set_and_get() {
        let mut o = Overlay::new();
        set1(&mut o, (3, 1), "hi");
        assert_eq!(o.get((3, 1)), Some("hi"));
        assert_eq!(o.get((3, 0)), None);
        assert_eq!(o.get((0, 1)), None);
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn overwriting_a_cell_does_not_double_count() {
        let mut o = Overlay::new();
        set1(&mut o, (0, 0), "a");
        set1(&mut o, (0, 0), "b");
        assert_eq!(o.get((0, 0)), Some("b"));
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn undo_restores_the_previous_value_then_removes_the_entry() {
        let mut o = Overlay::new();
        set1(&mut o, (0, 0), "a");
        set1(&mut o, (0, 0), "b");

        assert!(o.undo());
        assert_eq!(o.get((0, 0)), Some("a"));
        assert_eq!(o.len(), 1);

        assert!(o.undo());
        assert_eq!(o.get((0, 0)), None);
        assert!(o.is_empty());

        assert!(!o.undo());
    }

    #[test]
    fn a_range_fill_undoes_as_one_step() {
        let mut o = Overlay::new();
        let fill = (0..5).map(|row| ((row, 2), "0".to_string()));
        o.set(fill);
        assert_eq!(o.len(), 5);

        assert!(o.undo());
        assert!(o.is_empty(), "one fill, one undo");

        assert!(o.redo());
        assert_eq!(o.len(), 5);
        assert_eq!(o.get((4, 2)), Some("0"));
    }

    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut o = Overlay::new();
        set1(&mut o, (0, 0), "a");
        o.undo();
        assert!(o.can_redo());

        set1(&mut o, (1, 1), "b");
        assert!(!o.can_redo());
        assert!(!o.redo());
    }

    #[test]
    fn an_empty_batch_leaves_no_step() {
        let mut o = Overlay::new();
        o.set([]);
        assert!(!o.can_undo());
    }

    #[test]
    fn rows_are_visited_in_file_order() {
        let mut o = Overlay::new();
        set1(&mut o, (7, 0), "c");
        set1(&mut o, (2, 1), "b");
        set1(&mut o, (2, 0), "a");

        let seen: Vec<usize> = o.rows().map(|(row, _)| row).collect();
        assert_eq!(seen, [2, 7]);
        assert_eq!(o.row(2).unwrap().len(), 2);
    }

    #[test]
    fn clear_drops_history_too() {
        let mut o = Overlay::new();
        set1(&mut o, (0, 0), "a");
        o.clear();
        assert!(o.is_empty());
        assert!(!o.can_undo(), "undoing past a write would resurrect edits");
    }
}
