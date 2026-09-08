//! What a run of a document *is*, and what colour that makes it.
//!
//! Shared by every format the cell window can recognise, so a name is a name
//! whether it came from a JSON key or an XML element, and one place decides
//! what colour that is. The formats themselves hold no ratatui and no theme:
//! they say what they found, and this turns it into something to draw.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::Theme;

/// The kinds a formatter can hand back.
///
/// Named for what a run is rather than for the format it came from — a JSON
/// key and an XML element name are both the thing that says what follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A JSON key, an element name.
    Name,
    /// An attribute name: a name subordinate to the one above it.
    Attr,
    /// A quoted string, an attribute value.
    Str,
    Num,
    /// `true`, `false`, `null`.
    Lit,
    /// Ordinary text content, which is the document's prose rather than its
    /// structure and is drawn in the ordinary colour.
    Text,
    /// Braces, brackets, commas, angle brackets — and the indentation, which
    /// is structure too.
    Punct,
}

/// A run of text of one kind. `text` is a slice of the original value, or
/// whitespace the formatter added.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Piece {
    pub kind: Kind,
    pub text: String,
}

impl Piece {
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// A parsed document, coloured by what each run of it is.
pub fn lines(document: &[Vec<Piece>], theme: &Theme) -> Vec<Line<'static>> {
    document
        .iter()
        .map(|line| {
            Line::from(
                line.iter()
                    .map(|piece| {
                        let style = match piece.kind {
                            // Text is the document's own words: the same
                            // colour they would be if plv had recognised
                            // nothing, so recognising a format never changes
                            // how the reading itself looks.
                            Kind::Text => Style::new(),
                            Kind::Name => Style::new().fg(theme.syntax_key),
                            Kind::Attr => Style::new().fg(theme.syntax_attr),
                            Kind::Str => Style::new().fg(theme.syntax_string),
                            Kind::Num => Style::new().fg(theme.syntax_number),
                            Kind::Lit => Style::new().fg(theme.syntax_literal),
                            Kind::Punct => Style::new().fg(theme.syntax_punct),
                        };
                        Span::styled(piece.text.clone(), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}
