use ratatui::style::Color;

pub struct Theme {
    /// The row-number gutter, which sits behind the data rather than beside
    /// it: relative distances are read at a glance or not at all.
    pub row_num: Color,
    /// The current line's own number, which has to stand out from them.
    pub row_num_cursor: Color,
    pub header: Color,
    pub cursor_bg: Color,
    pub cursor_fg: Color,
    pub col_cursor_bg: Color,
    pub col_cursor_fg: Color,
    pub border: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub message_bg: Color,
    pub message_fg: Color,
    pub match_bg: Color,
    pub match_fg: Color,
    /// Cells holding an edit that has not been written to the file yet.
    pub edited_fg: Color,
    /// Empty cells, which are shown as a marker rather than as the word
    /// `null`: it has to read as "nothing here" and not as data.
    pub null_fg: Color,
    /// The candidate the completion panel has applied to the line.
    pub completion_bg: Color,
    pub completion_fg: Color,
    /// Cells inside a visual selection.
    pub selection_bg: Color,
    pub selection_fg: Color,
    /// Colours for a cell the window recognised as a document. Named for what
    /// a run of text *is* rather than for JSON, since the next format to be
    /// recognised will have keys, strings and numbers too.
    pub syntax_key: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_literal: Color,
    /// Braces, commas and indentation: structure, which is read past rather
    /// than read.
    pub syntax_punct: Color,
}

impl Theme {
    pub fn catppuccin_mocha() -> Self {
        Self {
            row_num: Color::Rgb(108, 112, 134),        // Overlay0
            row_num_cursor: Color::Rgb(249, 226, 175), // Yellow
            header: Color::Rgb(137, 180, 250),         // Blue
            cursor_bg: Color::Rgb(69, 71, 90),         // Surface1
            cursor_fg: Color::Rgb(205, 214, 244),      // Text
            col_cursor_bg: Color::Rgb(88, 91, 112),    // Surface2
            col_cursor_fg: Color::Rgb(205, 214, 244),  // Text
            border: Color::Rgb(88, 91, 112),           // Surface2
            status_bg: Color::Rgb(49, 50, 68),         // Surface0
            status_fg: Color::Rgb(205, 214, 244),      // Text
            message_bg: Color::Rgb(250, 179, 135),     // Peach
            message_fg: Color::Rgb(30, 30, 46),        // Base
            match_bg: Color::Rgb(249, 226, 175),       // Yellow
            match_fg: Color::Rgb(30, 30, 46),          // Base
            edited_fg: Color::Rgb(243, 139, 168),      // Red
            null_fg: Color::Rgb(108, 112, 134),        // Overlay0
            completion_bg: Color::Rgb(137, 180, 250),  // Blue
            completion_fg: Color::Rgb(30, 30, 46),     // Base
            selection_bg: Color::Rgb(88, 91, 112),     // Surface2
            selection_fg: Color::Rgb(205, 214, 244),   // Text
            syntax_key: Color::Rgb(137, 180, 250),     // Blue
            syntax_string: Color::Rgb(166, 227, 161),  // Green
            syntax_number: Color::Rgb(250, 179, 135),  // Peach
            syntax_literal: Color::Rgb(203, 166, 247), // Mauve
            syntax_punct: Color::Rgb(108, 112, 134),   // Overlay0
        }
    }
}
