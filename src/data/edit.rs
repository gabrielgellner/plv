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

use std::collections::{BTreeMap, BTreeSet};

/// A cell's position in the source file: the 0-based data row — the header is
/// not counted — and the field index.
pub type Cell = (usize, usize);

/// What one undoable step has to put back.
///
/// A step is a list rather than a single item because a fill or a `{n}dd` is
/// one action as far as the user is concerned, and `u` should take all of it
/// back at once.
#[derive(Debug)]
enum Undoable {
    /// The value the cell held before, or `None` if it held no pending edit.
    Cell(Cell, Option<String>),
    /// Whether the row was already struck out before.
    Struck(usize, bool),
    /// A row added before source row `before`, at this place among the rows
    /// already there, with this id.
    Added { before: usize, at: usize, id: usize },
}

type Change = Vec<Undoable>;

/// Pending edits, grouped by row.
///
/// Grouped because both readers want them that way: the renderer asks for the
/// rows in the current page, and the writer walks records in file order.
#[derive(Default)]
pub struct Overlay {
    rows: BTreeMap<usize, BTreeMap<usize, String>>,
    /// Source rows struck out, to be left out when the file is written.
    ///
    /// Struck rather than removed: the file is not touched until `:w`, so a
    /// deleted row is a note about what to leave out, and `u` puts it back.
    /// Sorted, because every reader wants to ask how many come before a
    /// given row.
    struck: BTreeSet<usize>,
    /// Rows added, keyed by the source row they go *before*, in the order
    /// they should appear there. The values are ids, not contents: a new row's
    /// cells live in `rows` like any other row's, under an id past the end of
    /// the file, so everything that already reads or writes a cell works on
    /// them unchanged.
    added: BTreeMap<usize, Vec<usize>>,
    /// Ids handed out so far. Never reused, so an undone row that is redone
    /// cannot collide with one added in between.
    issued: usize,
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
        self.len == 0 && self.struck.is_empty() && self.added.is_empty()
    }

    /// Everything pending, cells and struck rows together — what `[+n]` counts
    /// and what makes quitting ask first.
    pub fn pending(&self) -> usize {
        self.len + self.struck.len() + self.added_count()
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
            .map(|(cell, value)| Undoable::Cell(cell, self.insert(cell, value)))
            .collect();
        if change.is_empty() {
            return;
        }
        self.undo.push(change);
        self.redo.clear();
    }

    /// Delete rows: strike out the file's, take back the added ones.
    ///
    /// One undoable step covering both, since a `dd` over a run of rows
    /// should not care which of them were in the file. `beyond` is the file's
    /// row count, which is what tells the two apart.
    ///
    /// An added row is not in the file to strike out, so deleting it means
    /// taking it back out of the buffer — and its cells with it, or the write
    /// would find values with no row to put them in.
    pub fn delete<I: IntoIterator<Item = usize>>(&mut self, rows: I, beyond: usize) {
        let mut change = Change::new();
        for row in rows {
            if row < beyond {
                let was = !self.struck.insert(row);
                change.push(Undoable::Struck(row, was));
                continue;
            }
            let Some((before, at)) = self.locate(row) else {
                continue;
            };
            if let Some(cells) = self.rows.remove(&row) {
                self.len -= cells.len();
                for (col, value) in cells {
                    change.push(Undoable::Cell((row, col), Some(value)));
                }
            }
            if let Some(ids) = self.added.get_mut(&before) {
                ids.retain(|&other| other != row);
                if ids.is_empty() {
                    self.added.remove(&before);
                }
            }
            change.push(Undoable::Added {
                before,
                at,
                id: row,
            });
        }
        if change.is_empty() {
            return;
        }
        self.undo.push(change);
        self.redo.clear();
    }

    /// Whether a source row is struck out.
    pub fn is_struck(&self, row: usize) -> bool {
        self.struck.contains(&row)
    }

    /// Struck rows, in file order.
    pub fn struck(&self) -> impl Iterator<Item = usize> + '_ {
        self.struck.iter().copied()
    }

    /// How many struck rows come before `row`, which is what turns a display
    /// position back into a file row.
    pub fn struck_before(&self, row: usize) -> usize {
        self.struck.range(..row).count()
    }

    /// Struck rows in `range`.
    pub fn struck_in(&self, range: std::ops::Range<usize>) -> usize {
        self.struck.range(range).count()
    }

    pub fn struck_count(&self) -> usize {
        self.struck.len()
    }

    /// Add an empty row before source row `before`, and return its id.
    ///
    /// `beyond` is the file's row count: ids start past it, so a new row can
    /// never be mistaken for one that is in the file.
    pub fn add_row(&mut self, before: usize, at: usize, beyond: usize) -> usize {
        let id = beyond + self.issued;
        self.issued += 1;
        let group = self.added.entry(before).or_default();
        let at = at.min(group.len());
        group.insert(at, id);
        self.undo.push(vec![Undoable::Added { before, at, id }]);
        self.redo.clear();
        id
    }

    /// Where an added row sits: which source row it precedes, and its place
    /// among the rows added there.
    pub fn locate(&self, id: usize) -> Option<(usize, usize)> {
        self.added.iter().find_map(|(&before, ids)| {
            ids.iter()
                .position(|&other| other == id)
                .map(|at| (before, at))
        })
    }

    /// Rows added, by the source row they precede, in file order.
    pub fn added(&self) -> impl Iterator<Item = (usize, &[usize])> {
        self.added
            .iter()
            .map(|(&before, ids)| (before, ids.as_slice()))
    }

    /// Rows added before source row `before`.
    pub fn added_at(&self, before: usize) -> &[usize] {
        self.added.get(&before).map_or(&[], Vec::as_slice)
    }

    pub fn added_count(&self) -> usize {
        self.added.values().map(Vec::len).sum()
    }

    /// How many added rows come at or before source row `row`.
    pub fn added_before(&self, row: usize) -> usize {
        self.added.range(..=row).map(|(_, ids)| ids.len()).sum()
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
        self.struck.clear();
        self.added.clear();
        self.len = 0;
        self.undo.clear();
        self.redo.clear();
    }

    /// Restore the values in `change`, returning the change that would restore
    /// what was there before this call.
    fn revert(&mut self, change: Change) -> Change {
        change
            .into_iter()
            .map(|step| match step {
                Undoable::Cell(cell, previous) => {
                    let current = match previous {
                        Some(value) => self.insert(cell, value),
                        None => self.remove(cell),
                    };
                    Undoable::Cell(cell, current)
                }
                Undoable::Added { before, at, id } => {
                    // Undoing an addition takes the row out; redoing puts it
                    // back where it was rather than at the end, or a row added
                    // between two others would come back somewhere else.
                    let present = self.added.get(&before).is_some_and(|ids| ids.contains(&id));
                    if present {
                        if let Some(ids) = self.added.get_mut(&before) {
                            ids.retain(|&other| other != id);
                            if ids.is_empty() {
                                self.added.remove(&before);
                            }
                        }
                    } else {
                        let group = self.added.entry(before).or_default();
                        let at = at.min(group.len());
                        group.insert(at, id);
                    }
                    Undoable::Added { before, at, id }
                }
                Undoable::Struck(row, was) => {
                    let now = self.struck.contains(&row);
                    if was {
                        self.struck.insert(row);
                    } else {
                        self.struck.remove(&row);
                    }
                    Undoable::Struck(row, now)
                }
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
    fn striking_rows_is_one_undoable_step() {
        let mut o = Overlay::new();
        o.delete([2, 5, 9], usize::MAX);
        assert_eq!(o.struck_count(), 3);
        assert!(o.is_struck(5));
        assert!(!o.is_struck(4));
        assert_eq!(o.struck().collect::<Vec<_>>(), [2, 5, 9], "in file order");

        assert!(o.undo());
        assert_eq!(o.struck_count(), 0, "one dd, one undo");
        assert!(o.redo());
        assert!(o.is_struck(9));
    }

    #[test]
    fn striking_a_row_twice_does_not_double_count_or_undo_wrongly() {
        let mut o = Overlay::new();
        o.delete([3], usize::MAX);
        o.delete([3], usize::MAX);
        assert_eq!(o.struck_count(), 1);

        o.undo();
        assert!(o.is_struck(3), "the second strike was a no-op to take back");
        o.undo();
        assert!(!o.is_struck(3));
    }

    #[test]
    fn struck_before_is_what_turns_a_position_back_into_a_row() {
        let mut o = Overlay::new();
        o.delete([1, 4, 5], usize::MAX);
        assert_eq!(o.struck_before(0), 0);
        assert_eq!(o.struck_before(1), 0, "the row itself does not count");
        assert_eq!(o.struck_before(2), 1);
        assert_eq!(o.struck_before(5), 2);
        assert_eq!(o.struck_before(99), 3);
    }

    #[test]
    fn struck_rows_and_edited_cells_share_the_history() {
        let mut o = Overlay::new();
        set1(&mut o, (0, 0), "x");
        o.delete([7], usize::MAX);
        assert_eq!(o.pending(), 2, "one cell and one row");
        assert!(!o.is_empty());

        o.undo();
        assert!(!o.is_struck(7));
        assert_eq!(o.get((0, 0)), Some("x"), "the cell edit is untouched");
        o.undo();
        assert!(o.is_empty());
    }

    #[test]
    fn an_added_row_gets_an_id_past_the_end_of_the_file() {
        let mut o = Overlay::new();
        // A file of 3 rows: added rows are 3, 4, … so nothing collides with a
        // row that is actually in it.
        let first = o.add_row(1, usize::MAX, 3);
        let second = o.add_row(1, usize::MAX, 3);
        assert_eq!((first, second), (3, 4));
        assert_eq!(o.added_at(1), [3, 4], "in the order they were added");
        assert_eq!(o.added_count(), 2);
        assert_eq!(o.pending(), 2);
    }

    #[test]
    fn an_added_row_holds_its_cells_like_any_other() {
        let mut o = Overlay::new();
        let id = o.add_row(0, usize::MAX, 3);
        o.set([((id, 1), "typed".to_string())]);
        assert_eq!(o.get((id, 1)), Some("typed"));
        assert!(o.row(id).is_some(), "the renderer finds it the usual way");
    }

    #[test]
    fn adding_a_row_undoes_and_redoes() {
        let mut o = Overlay::new();
        let id = o.add_row(2, usize::MAX, 3);
        assert!(o.undo());
        assert_eq!(o.added_count(), 0);
        assert!(o.redo());
        assert_eq!(o.added_at(2), [id], "back where it was");
    }

    #[test]
    fn an_undone_row_does_not_hand_its_id_to_the_next_one() {
        let mut o = Overlay::new();
        let first = o.add_row(0, usize::MAX, 3);
        o.undo();
        let second = o.add_row(0, usize::MAX, 3);
        assert_ne!(first, second, "ids are never reused");
    }

    #[test]
    fn added_before_counts_what_comes_at_or_above_a_row() {
        let mut o = Overlay::new();
        o.add_row(0, usize::MAX, 10);
        o.add_row(4, usize::MAX, 10);
        o.add_row(4, usize::MAX, 10);
        assert_eq!(o.added_before(0), 1);
        assert_eq!(o.added_before(3), 1);
        assert_eq!(o.added_before(4), 3);
        assert_eq!(o.added_before(99), 3);
    }

    #[test]
    fn deleting_an_added_row_takes_it_back_rather_than_striking_it() {
        // Striking it would leave the writer looking for a record to drop
        // that was never in the file, and the write would be refused.
        let mut o = Overlay::new();
        let id = o.add_row(1, 0, 3);
        o.set([((id, 0), "typed".to_string())]);
        assert_eq!(o.pending(), 2);

        o.delete([id], 3);
        assert_eq!(o.added_count(), 0, "taken back out");
        assert_eq!(o.struck_count(), 0, "and not struck");
        assert_eq!(o.get((id, 0)), None, "its cells went with it");
        assert!(o.is_empty());
    }

    #[test]
    fn taking_back_an_added_row_undoes_with_its_cells() {
        let mut o = Overlay::new();
        let id = o.add_row(1, 0, 3);
        o.set([((id, 0), "typed".to_string())]);
        o.delete([id], 3);

        assert!(o.undo());
        assert_eq!(o.added_at(1), [id], "the row is back");
        assert_eq!(o.get((id, 0)), Some("typed"), "and so is what was in it");
    }

    #[test]
    fn one_delete_can_span_rows_of_the_file_and_added_ones() {
        let mut o = Overlay::new();
        let id = o.add_row(1, 0, 3);
        o.delete([0, id, 1], 3);
        assert_eq!(o.struck_count(), 2, "the file's rows are struck");
        assert_eq!(o.added_count(), 0, "the added one is taken back");

        assert!(o.undo(), "and all of it undoes together");
        assert_eq!(o.struck_count(), 0);
        assert_eq!(o.added_count(), 1);
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
