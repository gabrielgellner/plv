use ratatui::style::Color;

pub struct Theme {
    pub row_num: Color,
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
}

impl Theme {
    pub fn catppuccin_mocha() -> Self {
        Self {
            row_num: Color::Rgb(166, 173, 200),   // Subtext0
            header: Color::Rgb(137, 180, 250),     // Blue
            cursor_bg: Color::Rgb(69, 71, 90),       // Surface1
            cursor_fg: Color::Rgb(205, 214, 244),   // Text
            col_cursor_bg: Color::Rgb(88, 91, 112), // Surface2
            col_cursor_fg: Color::Rgb(205, 214, 244), // Text
            border: Color::Rgb(88, 91, 112),        // Surface2
            status_bg: Color::Rgb(49, 50, 68),     // Surface0
            status_fg: Color::Rgb(205, 214, 244),  // Text
            message_bg: Color::Rgb(250, 179, 135), // Peach
            message_fg: Color::Rgb(30, 30, 46),    // Base
            match_bg: Color::Rgb(249, 226, 175),   // Yellow
            match_fg: Color::Rgb(30, 30, 46),      // Base
        }
    }
}
