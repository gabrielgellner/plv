mod browser;
mod help;
mod panel;
mod prompt;
mod statusbar;
mod table;
pub mod theme;

pub use browser::{Browser, BrowserState};
pub use help::{Help, Section};
pub use panel::{CellView, Panel, cell_height, height as panel_height, wrap};
pub use prompt::Prompt;
pub use statusbar::StatusBar;
pub use table::{DataTable, MIN_COLUMN, Widths, col_offset_showing, drawn_width, natural_width};
pub use theme::Theme;

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum SelectionMode {
    #[default]
    Row,
    Column,
    Cell,
}

impl SelectionMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Row => Self::Column,
            Self::Column => Self::Cell,
            Self::Cell => Self::Row,
        }
    }
}
