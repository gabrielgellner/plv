use regex::Regex;

pub struct SearchQuery {
    pub raw: String,
    pub regex: Regex,
}

impl SearchQuery {
    pub fn new(raw: String) -> Result<Self, regex::Error> {
        let regex = Regex::new(&raw)?;
        Ok(Self { raw, regex })
    }
}

pub enum SearchStatus {
    /// Background scan still in progress; more matches may arrive.
    Searching,
    /// Scan finished; `matching_rows` is the complete result set.
    Complete,
}

pub struct SearchState {
    pub query: SearchQuery,
    /// Sorted absolute row indices (0-based) that contain at least one match.
    /// Grows as background chunks arrive.
    pub matching_rows: Vec<usize>,
    /// Index into `matching_rows` for the currently selected match.
    pub current_idx: usize,
    pub status: SearchStatus,
    /// Set to true once we've auto-jumped to the first match after the
    /// initial search results arrive.
    pub initial_jump_done: bool,
}

impl SearchState {
    pub fn new(query: SearchQuery) -> Self {
        Self {
            query,
            matching_rows: Vec::new(),
            current_idx: 0,
            status: SearchStatus::Searching,
            initial_jump_done: false,
        }
    }

    /// `(current_1based, total, complete)` — used by the status bar.
    /// Returns `(0, 0, false)` when no matches have arrived yet.
    pub fn match_info(&self) -> (usize, usize, bool) {
        let complete = matches!(self.status, SearchStatus::Complete);
        if self.matching_rows.is_empty() {
            (0, 0, complete)
        } else {
            (self.current_idx + 1, self.matching_rows.len(), complete)
        }
    }

    pub fn current_row(&self) -> Option<usize> {
        self.matching_rows.get(self.current_idx).copied()
    }

    /// Jump to the next match strictly after `cursor_row`, wrapping if needed.
    /// Uses the cursor position so manual navigation between `n` presses is respected.
    pub fn next_from(&mut self, cursor_row: usize) -> Option<usize> {
        if self.matching_rows.is_empty() {
            return None;
        }
        // partition_point gives the first index where row > cursor_row
        let idx = self.matching_rows.partition_point(|&r| r <= cursor_row);
        self.current_idx = idx % self.matching_rows.len();
        self.current_row()
    }

    /// Jump to the previous match strictly before `cursor_row`, wrapping if needed.
    pub fn prev_from(&mut self, cursor_row: usize) -> Option<usize> {
        if self.matching_rows.is_empty() {
            return None;
        }
        // partition_point gives the first index where r >= cursor_row;
        // the element before it is the last one strictly before cursor_row.
        let idx = self.matching_rows.partition_point(|&r| r < cursor_row);
        self.current_idx = idx.checked_sub(1).unwrap_or(self.matching_rows.len() - 1);
        self.current_row()
    }
}
