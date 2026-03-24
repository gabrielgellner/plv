mod prompt;
mod statusbar;
mod table;
pub mod theme;

pub use prompt::Prompt;
pub use statusbar::StatusBar;
pub use table::DataTable;
pub use theme::Theme;

#[derive(Clone, Copy, PartialEq, Default)]
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
