//! The cell window: one value, in full, in a window of its own.
//!
//! The strip above the status bar ([`super::CellView`]) is the peek that stays
//! on while the cursor walks down a column — a few lines, always the cell the
//! cursor is on. This is the other half: where a value is *read*. It is a
//! window and not a band because everything that makes a big cell hard needs
//! room and a scroll — it is longer than half a screen, it has line breaks of
//! its own, and one day it will be JSON or markdown asking to be formatted —
//! and taking that room out of the table would leave no table.
//!
//! Like the `?` overlay it covers rather than displaces, so nothing about the
//! layout underneath changes while it is up: closing it puts the screen back
//! exactly as it was, and the cursor has not moved.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::Theme;

/// Borders and a column of padding either side, which the value does not get
/// to write in.
const CHROME: u16 = 4;
/// Narrower than this and the window is all frame and no value, so it stops
/// shrinking and lets the terminal clip it instead.
const MIN_WIDTH: u16 = 24;
/// Two borders, a line of value, and room for the ladder in [`titles`] to
/// still have somewhere to go.
const MIN_HEIGHT: u16 = 5;

/// One cell, laid out to be read: `value` wrapped to the window's width and
/// scrolled to `scroll`.
pub struct CellWindow<'a> {
    pub name: &'a str,
    /// The column's dtype, as Polars named it — `str`, `i64`. It is in the
    /// title because a window is where you go to ask what a value *is*, and
    /// the header row cannot say it.
    pub dtype: &'a str,
    pub value: &'a str,
    /// First display line shown, counted in wrapped lines rather than in the
    /// value's own — what `j` moves is what is on screen.
    pub scroll: usize,
    pub theme: &'a Theme,
}

/// Break a value into the lines the window will draw.
///
/// Word-aware, unlike the strip's [`super::wrap`], which cuts on the width:
/// the strip shows two or three lines of a value that is mostly off screen
/// anyway, where a hard break is honest about the truncation, but this is the
/// view that claims to be showing the whole thing, and prose broken mid-word
/// down a wide window is unreadable. A run with no space in it — a URL, a
/// base64 blob, a line of JSON — has nowhere to break and is cut on width, as
/// it must be.
///
/// The value's own newlines are kept: a quoted field can hold them and they
/// are the author's. So is leading whitespace, which is indentation in
/// anything that will later be formatted as code.
pub fn wrap(value: &str, width: u16) -> Vec<String> {
    let width = (width as usize).max(1);
    let mut lines = Vec::new();
    for source in value.split('\n') {
        let mut current = String::new();
        // Tracked rather than recounted: a long line would otherwise cost a
        // pass over what is already in hand for every word in it.
        let mut have = 0usize;
        // Each chunk keeps the space that ended it, so the break lands after
        // a word and runs of spaces survive.
        for chunk in source.split_inclusive(' ') {
            let len = chunk.chars().count();
            if have + len <= width {
                current.push_str(chunk);
                have += len;
                continue;
            }
            if have > 0 {
                lines.push(std::mem::take(&mut current));
                have = 0;
            }
            if len <= width {
                current.push_str(chunk);
                have = len;
                continue;
            }
            for c in chunk.chars() {
                if have == width {
                    lines.push(std::mem::take(&mut current));
                    have = 0;
                }
                current.push(c);
                have += 1;
            }
        }
        // Always, so an empty line in the value is still a line on screen.
        lines.push(current);
    }
    lines
}

/// Where the window sits on `area`, and how `value` breaks into lines inside
/// it.
///
/// Both answers come from here, so the renderer and the key handler that
/// clamps the scroll cannot disagree about how many lines there are.
///
/// It sizes to the value up to a cap — wide, because that is what was asked
/// for, but never the whole screen: a window that reaches the edges is a
/// screen, and the table around it is what says where the value came from.
/// `title` is counted in, so a window is never too narrow to say what it is
/// showing; a one-word value still gets a border that names its column.
pub fn window(value: &str, title: &str, area: Rect) -> (Rect, Vec<String>) {
    let longest = value
        .split('\n')
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);

    let widest = fraction(area.width, 9, 10).clamp(MIN_WIDTH.min(area.width), area.width);
    let width = (longest + CHROME as usize)
        .max(title.chars().count() + 2)
        .min(widest as usize)
        .max(MIN_WIDTH.min(area.width) as usize) as u16;

    let lines = wrap(value, width.saturating_sub(CHROME));

    let tallest = fraction(area.height, 4, 5).clamp(MIN_HEIGHT.min(area.height), area.height);
    let height = (lines.len() + 2)
        .min(tallest as usize)
        .max(MIN_HEIGHT.min(area.height) as usize) as u16;

    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    (rect, lines)
}

/// `n * num / den`, in wide arithmetic — a terminal is never large enough for
/// this to matter and the overflow if one ever were is not worth the doubt.
fn fraction(n: u16, num: u32, den: u32) -> u16 {
    (n as u32 * num / den) as u16
}

/// Lines of value the window shows at once.
pub fn text_rows(window: Rect) -> usize {
    window.height.saturating_sub(2) as usize
}

/// The furthest the value scrolls: the last line at the bottom of the window,
/// not off the top of it. Scrolling into blank space loses the end of the
/// value, which is the thing being scrolled towards.
pub fn max_scroll(lines: usize, rows: usize) -> usize {
    lines.saturating_sub(rows.max(1))
}

/// What the top border says, longest first.
///
/// The name is the one part that cannot be dropped — it is what says which
/// value this is — so the dtype goes first and then the size, and the name is
/// left holding the border on its own. Nothing here counts lines: the line
/// count is a fact about the wrapping, which is a fact about the width, which
/// this decides — and the footer says it anyway, in the numbers you actually
/// scroll by.
fn titles(name: &str, dtype: &str, chars: usize) -> [String; 3] {
    let plural = if chars == 1 { "" } else { "s" };
    [
        format!(" {name} — {dtype}, {chars} character{plural} "),
        format!(" {name} — {chars} character{plural} "),
        format!(" {name} "),
    ]
}

/// The widest form of the title, for [`window`] to size itself against.
pub fn title(name: &str, dtype: &str, value: &str) -> String {
    let [widest, ..] = titles(name, dtype, value.chars().count());
    widest
}

/// The widest of `options` that fits `width`, or the last one if none do —
/// something has to be drawn, and the shortest form is the closest to true.
fn fitting(options: &[String], width: u16) -> String {
    options
        .iter()
        .find(|text| text.chars().count() <= width as usize)
        .unwrap_or_else(|| options.last().expect("never empty"))
        .clone()
}

impl Widget for CellWindow<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ladder = titles(self.name, self.dtype, self.value.chars().count());
        let (popup, lines) = window(self.value, &ladder[0], area);
        if popup.width == 0 || popup.height == 0 {
            return;
        }
        let rows = text_rows(popup);
        let scroll = self.scroll.min(max_scroll(lines.len(), rows));
        let title = fitting(&ladder, popup.width.saturating_sub(2));

        // The keys are on the frame rather than in the value's room: the
        // window is opened to read one thing, and a line of help taking a
        // line of the value would be the strip's problem all over again.
        //
        // Where the value fits, there is no position to report and saying
        // `1–4 of 4` would be answering a question nobody has. Where it does
        // not, the position is the thing that cannot be guessed from the
        // screen, so the keys give way to it and not the other way round.
        let footer = if lines.len() > rows {
            let (first, last, total) = (scroll + 1, (scroll + rows).min(lines.len()), lines.len());
            fitting(
                &[
                    format!(" {first}–{last} of {total}   j/k ^d/^u g/G   q close "),
                    format!(" {first}–{last} of {total}   j/k  q close "),
                    format!(" {first}–{last} of {total} "),
                ],
                popup.width.saturating_sub(2),
            )
        } else {
            fitting(
                &[" q close ".to_string(), " q ".to_string()],
                popup.width.saturating_sub(2),
            )
        };

        Clear.render(popup, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.border))
            .title(Span::styled(
                title,
                Style::new()
                    .fg(self.theme.header)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                footer,
                Style::new().fg(self.theme.row_num).add_modifier(
                    // Dim, because it is the same six keys every time.
                    Modifier::ITALIC,
                ),
            ));
        let inner = block.inner(popup);
        block.render(popup, buf);

        // A column of padding either side, which `window` has already paid
        // for in `CHROME`.
        let text = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };

        // An empty cell is drawn the way the table draws one: a marker that
        // reads as nothing-here rather than as the word `null`, which is a
        // value a field can genuinely hold.
        if self.value.is_empty() {
            Paragraph::new(Line::from(Span::styled(
                "·",
                Style::new().fg(self.theme.null_fg),
            )))
            .render(text, buf);
            return;
        }

        let shown: Vec<Line> = lines
            .iter()
            .skip(scroll)
            .take(rows)
            .map(|line| Line::raw(line.clone()))
            .collect();
        Paragraph::new(shown).render(text, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &str, area: Rect) -> Vec<String> {
        render_at(value, area, 0)
    }

    fn render_at(value: &str, area: Rect, scroll: usize) -> Vec<String> {
        let theme = Theme::catppuccin_mocha();
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            value,
            scroll,
            theme: &theme,
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn wrapping_breaks_at_spaces() {
        assert_eq!(
            wrap("the quick brown fox", 10),
            ["the quick ", "brown fox"],
            "and not mid-word"
        );
    }

    #[test]
    fn a_word_with_nowhere_to_break_is_cut_on_the_width() {
        // A URL or a base64 blob has no space in it; the alternative to
        // cutting is a line wider than the window.
        assert_eq!(wrap("aaaaaaaa", 3), ["aaa", "aaa", "aa"]);
        assert_eq!(wrap("hi aaaaaa", 3), ["hi ", "aaa", "aaa"]);
    }

    #[test]
    fn wrapping_keeps_the_values_own_lines_and_indentation() {
        assert_eq!(wrap("{\n  \"a\": 1\n}", 40), ["{", "  \"a\": 1", "}"]);
        assert_eq!(wrap("a\n\nb", 40), ["a", "", "b"], "a blank line survives");
        assert_eq!(wrap("", 40), [""], "and an empty value is one line");
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        assert_eq!(wrap("éàü", 2), ["éà", "ü"]);
    }

    #[test]
    fn the_window_sizes_to_the_value_but_not_past_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let (small, _) = window("short", " note ", area);
        assert_eq!(small.width, MIN_WIDTH, "a short value still gets a window");
        assert_eq!(small.height, MIN_HEIGHT);

        let (big, lines) = window(&"x".repeat(10_000), " note ", area);
        assert!(big.width <= 72, "nine tenths of the width at most: {big:?}");
        assert!(big.height <= 19, "four fifths of the height: {big:?}");
        assert!(lines.len() > text_rows(big), "so it has to scroll");
    }

    /// A window narrower than its own title would be a border that cannot say
    /// what it is showing.
    #[test]
    fn the_window_is_never_narrower_than_what_it_has_to_say() {
        let title = title("a-long-column-name", "str", "hi");
        let (popup, _) = window("hi", &title, Rect::new(0, 0, 80, 24));
        assert!(
            popup.width as usize >= title.chars().count() + 2,
            "{popup:?} for {title:?}"
        );
    }

    #[test]
    fn the_window_is_centred() {
        let area = Rect::new(0, 0, 80, 24);
        let (popup, _) = window("hello", " note ", area);
        assert_eq!(popup.x, (area.width - popup.width) / 2);
        assert_eq!(popup.y, (area.height - popup.height) / 2);
    }

    #[test]
    fn a_terminal_smaller_than_the_window_is_not_overflowed() {
        let area = Rect::new(0, 0, 10, 3);
        let (popup, _) = window("a value that will not fit", " note ", area);
        assert!(
            popup.width <= area.width && popup.height <= area.height,
            "{popup:?}"
        );
        // And it still draws rather than panicking on a zero-width inner.
        render("a value that will not fit", area);
    }

    #[test]
    fn the_last_line_scrolls_to_the_bottom_and_no_further() {
        assert_eq!(
            max_scroll(30, 10),
            20,
            "line 21 at the top, 30 at the bottom"
        );
        assert_eq!(max_scroll(4, 10), 0, "a value that fits does not scroll");
    }

    #[test]
    fn the_title_names_the_column_the_type_and_the_size() {
        let drawn = render("hello world", Rect::new(0, 0, 60, 12));
        let top = drawn.iter().find(|line| line.contains('┌')).unwrap();
        assert!(top.contains("note"), "{top}");
        assert!(top.contains("str"), "{top}");
        assert!(top.contains("11 characters"), "{top}");
    }

    #[test]
    fn a_narrow_window_gives_up_the_sizes_before_the_name() {
        let ladder = titles("note", "str", 11);
        assert_eq!(fitting(&ladder, 60), ladder[0]);
        assert_eq!(fitting(&ladder, 23), ladder[1]);
        assert_eq!(fitting(&ladder, 10), ladder[2]);
        assert_eq!(fitting(&ladder, 2), ladder[2], "something has to be drawn");
    }

    #[test]
    fn scrolling_moves_the_value_and_the_footer_says_where_it_is() {
        let value: String = (1..=40).map(|n| format!("line{n}\n")).collect();
        let area = Rect::new(0, 0, 60, 14);
        let (popup, lines) = window(&value, &title("note", "str", &value), area);
        let rows = text_rows(popup);
        let bottom = (popup.y + popup.height - 1) as usize;

        let top = render(&value, area);
        assert!(top.iter().any(|l| l.contains("line1 ")), "{top:?}");

        let down = render_at(&value, area, 5);
        assert!(!down.iter().any(|l| l.contains("line1 ")), "{down:?}");
        assert!(down.iter().any(|l| l.contains("line6")), "{down:?}");
        assert!(
            down[bottom].contains(&format!("6–{} of {}", 5 + rows, lines.len())),
            "{}",
            down[bottom]
        );

        // Past the end, the last line sits at the bottom rather than the value
        // scrolling out of sight.
        let end = render_at(&value, area, 9_999);
        assert!(end.iter().any(|l| l.contains("line40")), "{end:?}");
        assert!(
            end[bottom].contains(&format!("of {}", lines.len())),
            "{}",
            end[bottom]
        );
    }

    #[test]
    fn an_empty_cell_reads_as_nothing_here() {
        let drawn = render("", Rect::new(0, 0, 40, 8));
        assert!(drawn.iter().any(|line| line.contains('·')), "{drawn:?}");
        assert!(!drawn.iter().any(|line| line.contains("null")), "{drawn:?}");
    }
}
