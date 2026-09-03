//! The column picker: choosing what is on show without naming every column.
//!
//! `-` hides the cursor column and `:select` names the ones to keep, which
//! between them cover narrowing a table down. Neither covers the other
//! direction — no key un-hides *one* column, because naming it is the only way
//! to say which — so assembling a working set out of a hundred columns means
//! typing every name. This is the list you toggle instead.
//!
//! It holds a **working copy** of what it is editing, so `Esc` costs nothing
//! and `Enter` is the only thing that changes the view. What it produces is a
//! `view::Command::Select`, applied through the same path `:select` and `-`
//! take: there is one answer to which columns show, not a second one living
//! here.

use std::collections::BTreeSet;

use ratatui::layout::Constraint;

use crate::ui::BrowserState;

/// What a ticked box looks like. A blank rather than a second mark for the
/// off state: the ticks are being scanned down a column, and two marks make
/// that a reading problem rather than a glance.
const TICK: &str = "✓";

pub struct Picker {
    /// Source column indices in the order they are listed: the view's own
    /// order first, then the columns it hides.
    ///
    /// The view's order and not the file's, because `:select c a` sets an
    /// order as well as a set — listing in file order and writing back in
    /// file order would silently undo that, and a picker that quietly
    /// discards state is worse than no picker.
    order: Vec<usize>,
    /// Every column's name, indexed by source column.
    names: Vec<String>,
    shown: BTreeSet<usize>,
    pinned: BTreeSet<usize>,
    pub state: BrowserState,
}

impl Picker {
    /// `view_order` is what `View::columns` gives: the source columns on show,
    /// in the order they are drawn.
    pub fn new(names: Vec<String>, view_order: Vec<usize>, pinned: BTreeSet<usize>) -> Self {
        let shown: BTreeSet<usize> = view_order.iter().copied().collect();
        // The hidden ones go after, in the file's own order — there is no
        // other order they could be in, since the view does not hold them.
        let order: Vec<usize> = view_order
            .into_iter()
            .chain((0..names.len()).filter(|col| !shown.contains(col)))
            .collect();
        Self {
            order,
            names,
            shown,
            pinned,
            state: BrowserState::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Only ever true of a file with no columns at all, which nothing else in
    /// plv can open — but the cursor arithmetic reads better for having asked.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The source column the cursor is on.
    pub fn cursor_column(&self) -> Option<usize> {
        self.order.get(self.state.selected).copied()
    }

    /// Toggle whether the cursor column is on show.
    ///
    /// Refuses to untick the last one. `View::apply` refuses that too and is
    /// the backstop, but finding out on `Enter` that a whole session of
    /// ticking cannot be applied is worse than being stopped at the tick that
    /// caused it.
    pub fn toggle_shown(&mut self) -> Result<(), String> {
        let Some(column) = self.cursor_column() else {
            return Ok(());
        };
        if self.shown.contains(&column) {
            if self.shown.len() == 1 {
                return Err("that would hide every column".to_string());
            }
            self.shown.remove(&column);
        } else {
            self.shown.insert(column);
        }
        Ok(())
    }

    /// `a`: put every column back on show.
    pub fn show_all(&mut self) {
        self.shown = self.order.iter().copied().collect();
    }

    /// `A`: hide everything but the column under the cursor.
    ///
    /// Not a toggle paired with `a`, because the state it would toggle on is
    /// a hundred rows long and mostly off screen — a key whose direction
    /// depends on what you cannot see is a guess. Two keys, each doing one
    /// thing, is the honest shape for an operation over the whole list.
    ///
    /// Something has to stay, since a view with no columns is not one, and
    /// the cursor's is what to keep: this key exists for picking a handful
    /// out of a hundred, and the handful is built up from where you are
    /// already looking. On a hidden column it shows that one, which is the
    /// same rule read the other way.
    pub fn show_only_cursor(&mut self) {
        let Some(column) = self.cursor_column() else {
            return;
        };
        self.shown.clear();
        self.shown.insert(column);
    }

    /// Toggle whether the cursor column is pinned.
    ///
    /// Allowed on a hidden column: a pin is kept against the source column and
    /// waits for the view to show it again, so ticking both here is a way of
    /// saying what the column should look like when it comes back. Whether the
    /// pins fit is a question about the screen, and is asked once on `Enter`
    /// against the view that is actually adopted.
    pub fn toggle_pinned(&mut self) {
        let Some(column) = self.cursor_column() else {
            return;
        };
        if !self.pinned.remove(&column) {
            self.pinned.insert(column);
        }
    }

    /// The columns to select, in the order they are listed.
    pub fn selection(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|column| self.shown.contains(column))
            .collect()
    }

    /// The pins as they stand, for the app to adopt.
    pub fn pins(&self) -> BTreeSet<usize> {
        self.pinned.clone()
    }

    pub fn title(&self) -> String {
        format!(" columns — {} of {} shown ", self.shown.len(), self.len())
    }

    pub fn headers() -> &'static [&'static str] {
        &["column", "shown", "pinned"]
    }

    /// A fixed name column rather than one that grows into the space.
    ///
    /// The ticks are read *against* the names, and a `Min` first column pushes
    /// them to the far right of a wide terminal where the eye has to travel
    /// the width of the screen for every row. Names past this are truncated,
    /// as they are in the table itself.
    pub fn widths() -> &'static [Constraint] {
        &[
            Constraint::Length(32),
            Constraint::Length(7),
            Constraint::Length(8),
        ]
    }

    pub fn rows(&self) -> Vec<Vec<String>> {
        self.order
            .iter()
            .map(|&column| {
                let tick = |on: bool| if on { TICK.to_string() } else { String::new() };
                vec![
                    self.names
                        .get(column)
                        .cloned()
                        .unwrap_or_else(|| column.to_string()),
                    tick(self.shown.contains(&column)),
                    tick(self.pinned.contains(&column)),
                ]
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect()
    }

    /// The list opens in the view's order, not the file's, with whatever the
    /// view hides appended — so a reordering `:select` survives being looked at.
    #[test]
    fn the_list_opens_in_view_order_with_the_hidden_ones_after() {
        let picker = Picker::new(names(), vec![2, 0], BTreeSet::new());
        let listed: Vec<String> = picker
            .rows()
            .into_iter()
            .map(|row| row[0].clone())
            .collect();
        assert_eq!(listed, ["c", "a", "b", "d"]);
        assert_eq!(picker.selection(), [2, 0], "and applying changes nothing");
    }

    #[test]
    fn space_toggles_a_column_in_and_out_of_the_selection() {
        let mut picker = Picker::new(names(), vec![0, 1, 2, 3], BTreeSet::new());
        picker.state.go_to(1, 4); // onto b
        picker.toggle_shown().unwrap();
        assert_eq!(picker.selection(), [0, 2, 3]);

        picker.toggle_shown().unwrap();
        assert_eq!(picker.selection(), [0, 1, 2, 3], "and back again");
    }

    /// A column brought back goes to the position it is listed at, which is
    /// where the user is looking when they tick it.
    #[test]
    fn a_column_comes_back_where_it_is_listed() {
        let mut picker = Picker::new(names(), vec![0, 2], BTreeSet::new());
        // Listed a c b d: the view's 0 and 2, then the hidden 1 and 3.
        picker.state.go_to(2, 4); // onto b, the first hidden one
        picker.toggle_shown().unwrap();
        assert_eq!(picker.selection(), [0, 2, 1], "in its listed position");
    }

    /// Picking four columns out of two hundred means starting from none.
    #[test]
    fn a_and_shift_a_work_over_the_whole_list() {
        let mut picker = Picker::new(names(), vec![0, 1, 2, 3], BTreeSet::new());
        picker.state.go_to(2, 4); // onto c

        picker.show_only_cursor();
        assert_eq!(picker.selection(), [2], "everything but the cursor column");

        // And then the handful is built up from there.
        picker.state.go_to(0, 4);
        picker.toggle_shown().unwrap();
        assert_eq!(picker.selection(), [0, 2], "in listed order");

        picker.show_all();
        assert_eq!(picker.selection(), [0, 1, 2, 3]);
    }

    /// Keeping the cursor column means showing it when it was hidden, which
    /// is the same rule read the other way.
    #[test]
    fn shift_a_on_a_hidden_column_leaves_that_one_showing() {
        let mut picker = Picker::new(names(), vec![0, 1], BTreeSet::new());
        picker.state.go_to(3, 4); // onto d, which the view hides
        picker.show_only_cursor();
        assert_eq!(picker.selection(), [3]);
    }

    /// `A` lands on exactly one column, which is the fewest a view may have —
    /// so it never produces a state `toggle_shown` would have refused.
    #[test]
    fn shift_a_leaves_a_view_that_is_still_legal() {
        let mut picker = Picker::new(names(), vec![0, 1, 2, 3], BTreeSet::new());
        picker.show_only_cursor();
        assert_eq!(picker.selection().len(), 1);
        assert!(picker.toggle_shown().is_err(), "and the last one still holds");
    }

    #[test]
    fn unticking_the_last_column_is_refused() {
        let mut picker = Picker::new(names(), vec![0], BTreeSet::new());
        let refusal = picker.toggle_shown().unwrap_err();
        assert!(refusal.contains("hide every column"), "{refusal}");
        assert_eq!(picker.selection(), [0], "and it stays on show");
    }

    /// A pin on a hidden column is a statement about what it looks like when
    /// it comes back, which is exactly how pins already behave.
    #[test]
    fn a_hidden_column_can_still_be_pinned() {
        let mut picker = Picker::new(names(), vec![0], BTreeSet::new());
        picker.state.go_to(1, 4); // onto b, hidden
        picker.toggle_pinned();
        assert_eq!(picker.pins().into_iter().collect::<Vec<_>>(), [1]);

        let marks = &picker.rows()[1];
        assert_eq!(marks[1], "", "not shown");
        assert_eq!(marks[2], TICK, "but pinned");
    }

    #[test]
    fn pins_come_in_from_the_app_and_toggle_off_again() {
        let mut picker = Picker::new(names(), vec![0, 1], [0].into_iter().collect());
        assert_eq!(picker.rows()[0][2], TICK, "already pinned when it opens");
        picker.toggle_pinned();
        assert!(picker.pins().is_empty());
    }
}
