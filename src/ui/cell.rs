//! The cell window: one value, in full, in a window of its own.
//!
//! The strip above the status bar ([`super::CellView`]) is the peek that stays
//! on while the cursor walks down a column — a few lines, always the cell the
//! cursor is on. This is the other half: where a value is *read*. It is a
//! window and not a band because everything that makes a big cell hard needs
//! room and a scroll — it is longer than half a screen, it has line breaks of
//! its own, it is a JSON document whose structure is its content — and taking
//! that room out of the table would leave no table.
//!
//! Like the `?` overlay it covers rather than displaces, so nothing about the
//! layout underneath changes while it is up: closing it puts the screen back
//! exactly as it was, and the cursor has not moved.
//!
//! What it draws is a [`Content`]: the value's display lines, already
//! formatted and coloured, worked out once when the window opens rather than
//! on every keypress. The cursor cannot move while the window is up, so the
//! value cannot change under it.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

use super::{Theme, json, markup, syntax};

/// Borders and a column of padding either side, which the value does not get
/// to write in.
const CHROME: u16 = 4;
/// Narrower than this and the window is all frame and no value, so it stops
/// shrinking and lets the terminal clip it instead.
const MIN_WIDTH: u16 = 24;
/// Two borders, a line of value, and room for the ladder in [`titles`] to
/// still have somewhere to go.
const MIN_HEIGHT: u16 = 5;

/// What plv recognised the value as.
///
/// Deliberately not a guess. A cell shown as pretty JSON that was not JSON is
/// a lie about the data, and a viewer that lies once cannot be trusted for
/// the rest of the session — worse than plain text, which is at least honest
/// about being unstructured. So a format is claimed only when a real parse of
/// the whole value succeeded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Text,
    Json,
    /// XML or HTML, told apart by which of the two's rules the document
    /// needed — see [`markup::Flavour`].
    Markup(markup::Flavour),
}

impl Format {
    /// What the title calls it, or `None` where there is nothing to say.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Format::Text => None,
            Format::Json => Some("json"),
            Format::Markup(flavour) => Some(flavour.label()),
        }
    }
}

/// One cell, ready to draw: its display lines before wrapping, and what plv
/// made of it.
pub struct Content {
    pub format: Format,
    /// Showing the bytes rather than the formatting. The formatted view is an
    /// interpretation, and the raw value is the thing that gets edited and
    /// written, so there has to be a way back to it.
    pub raw: bool,
    pub lines: Vec<Line<'static>>,
    /// Characters in the *value*, not in what is drawn: the title is a fact
    /// about the cell, and formatting adds whitespace that is plv's own.
    pub chars: usize,
    empty: bool,
}

impl Content {
    pub fn new(value: &str, raw: bool, theme: &Theme) -> Self {
        // Parsed either way, so a raw view still knows what it is looking at
        // and can offer the way back. It is one parse per open or toggle, not
        // one per keypress.
        //
        // The formats are tried in turn, and each is a whole parse of the
        // value rather than a look at its first character — so nothing is
        // claimed on the strength of a `{` or a `<`. They cannot both
        // succeed: a JSON document does not begin with a tag.
        let (format, document) = match json::reindent(value) {
            Some(document) => (Format::Json, Some(document)),
            None => match markup::reindent(value) {
                Some((document, flavour)) => (Format::Markup(flavour), Some(document)),
                None => (Format::Text, None),
            },
        };
        let lines = match document {
            Some(document) if !raw => syntax::lines(&document, theme),
            _ => plain(value),
        };
        Self {
            format,
            raw,
            lines,
            chars: value.chars().count(),
            empty: value.is_empty(),
        }
    }

    /// Whether there is another way to look at this value — which is what
    /// decides whether the window offers the key.
    pub fn switchable(&self) -> bool {
        self.format != Format::Text
    }
}

/// A value with no structure to show: its own lines, and nothing added.
fn plain(value: &str) -> Vec<Line<'static>> {
    value
        .split('\n')
        .map(|line| Line::raw(line.to_string()))
        .collect()
}

/// Characters on a line, counted the way the wrap counts them.
fn line_chars(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

/// Break display lines into lines that fit `width`, keeping their styling.
///
/// Word-aware, unlike the strip's [`super::wrap`], which cuts on the width:
/// the strip shows two or three lines of a value that is mostly off screen
/// anyway, where a hard break is honest about the truncation, but this is the
/// view that claims to be showing the whole thing, and prose broken mid-word
/// down a wide window is unreadable. A run with no space in it — a URL, a
/// base64 blob — has nowhere to break and is cut on width, as it must be.
///
/// A wrapped line picks up its own indentation again, so the continuation of
/// a deep line in a document stays visibly inside it rather than starting
/// back at the left edge where a new key would.
///
/// It works on styled lines and not on a string because by the time a value
/// has been recognised as a document, its colours are part of what it says.
pub fn wrap(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper {
        width: (width as usize).max(1),
        indent: 0,
        out: Vec::new(),
        current: Vec::new(),
        have: 0,
    };
    for line in lines {
        wrapper.line(line);
    }
    wrapper.out
}

struct Wrapper {
    width: usize,
    /// What a continuation of the current line starts with.
    indent: usize,
    out: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    have: usize,
}

impl Wrapper {
    fn line(&mut self, line: &Line<'static>) {
        // Half the window at most: past that the hanging indent is taking the
        // room it was meant to be clarifying.
        self.indent = leading_spaces(line).min(self.width / 2);
        for span in &line.spans {
            // Each chunk keeps the space that ended it, so the break lands
            // after a word and runs of spaces survive.
            for chunk in span.content.split_inclusive(' ') {
                self.chunk(chunk, span.style);
            }
        }
        // Always, so an empty line in the value is still a line on screen.
        self.out.push(Line::from(std::mem::take(&mut self.current)));
        self.have = 0;
    }

    fn chunk(&mut self, chunk: &str, style: Style) {
        let len = chunk.chars().count();
        if self.have + len <= self.width {
            self.push(chunk.to_string(), style);
            return;
        }
        // Nothing but the hanging indent on this line means a break would not
        // help: the chunk has to be cut instead.
        if self.have > self.indent {
            self.wrap_here();
        }
        if self.have + len <= self.width {
            self.push(chunk.to_string(), style);
            return;
        }
        let chars: Vec<char> = chunk.chars().collect();
        let mut at = 0;
        while at < chars.len() {
            if self.have >= self.width {
                self.wrap_here();
            }
            let take = (self.width - self.have).min(chars.len() - at);
            self.push(chars[at..at + take].iter().collect(), style);
            at += take;
        }
    }

    fn push(&mut self, text: String, style: Style) {
        self.have += text.chars().count();
        self.current.push(Span::styled(text, style));
    }

    fn wrap_here(&mut self) {
        self.out.push(Line::from(std::mem::take(&mut self.current)));
        self.have = 0;
        if self.indent > 0 {
            self.push(" ".repeat(self.indent), Style::new());
        }
    }
}

fn leading_spaces(line: &Line<'_>) -> usize {
    let mut count = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            if c != ' ' {
                return count;
            }
            count += 1;
        }
    }
    count
}

/// Where the window sits on `area`, and how the content breaks into lines
/// inside it.
///
/// Both answers come from here, so the renderer and the key handler that
/// clamps the scroll cannot disagree about how many lines there are.
///
/// It sizes to the content up to a cap — wide, because that is what was asked
/// for, but never the whole screen: a window that reaches the edges is a
/// screen, and the table around it is what says where the value came from.
/// `title` is counted in, so a window is never too narrow to say what it is
/// showing; a one-word value still gets a border that names its column.
pub fn window(lines: &[Line<'static>], title: &str, area: Rect) -> (Rect, Vec<Line<'static>>) {
    let longest = lines.iter().map(line_chars).max().unwrap_or(0);

    let widest = fraction(area.width, 9, 10).clamp(MIN_WIDTH.min(area.width), area.width);
    let width = (longest + CHROME as usize)
        .max(title.chars().count() + 2)
        .min(widest as usize)
        .max(MIN_WIDTH.min(area.width) as usize) as u16;

    let wrapped = wrap(lines, width.saturating_sub(CHROME));

    let tallest = fraction(area.height, 4, 5).clamp(MIN_HEIGHT.min(area.height), area.height);
    let height = (wrapped.len() + 2)
        .min(tallest as usize)
        .max(MIN_HEIGHT.min(area.height) as usize) as u16;

    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    (rect, wrapped)
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
/// value this is — so the type goes first and then the size, and the name is
/// left holding the border on its own. Nothing here counts lines: the line
/// count is a fact about the wrapping, which is a fact about the width, which
/// this decides — and the footer says it anyway, in the numbers you actually
/// scroll by.
///
/// A recognised format takes the dtype's place rather than sitting beside it.
/// Every JSON cell is a `str` column, so the dtype is the part that says
/// nothing, and the reader has to be told which of the two things they are
/// looking at — a reformat the title did not own up to would leave the file's
/// own line breaks indistinguishable from plv's.
fn titles(name: &str, dtype: &str, content: &Content) -> [String; 3] {
    let plural = if content.chars == 1 { "" } else { "s" };
    let what = match (content.format.label(), content.raw) {
        (Some(label), false) => label.to_string(),
        (Some(label), true) => format!("{label}, raw"),
        (None, _) => dtype.to_string(),
    };
    [
        format!(" {name} — {what}, {} character{plural} ", content.chars),
        format!(" {name} — {} character{plural} ", content.chars),
        format!(" {name} "),
    ]
}

/// The widest form of the title, for [`window`] to size itself against.
pub fn title(name: &str, dtype: &str, content: &Content) -> String {
    let [widest, ..] = titles(name, dtype, content);
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

/// One cell, laid out to be read: `content` wrapped to the window's width and
/// scrolled to `scroll`.
pub struct CellWindow<'a> {
    pub name: &'a str,
    /// The column's dtype, as Polars named it — `str`, `i64`. It is in the
    /// title because a window is where you go to ask what a value *is*, and
    /// the header row cannot say it.
    pub dtype: &'a str,
    pub content: &'a Content,
    /// First display line shown, counted in wrapped lines rather than in the
    /// value's own — what `j` moves is what is on screen.
    pub scroll: usize,
    pub theme: &'a Theme,
}

impl Widget for CellWindow<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ladder = titles(self.name, self.dtype, self.content);
        let (popup, lines) = window(&self.content.lines, &ladder[0], area);
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
        //
        // `r` is named where there is another view to switch to, and only
        // there: a key that does nothing is worse than one that is not
        // offered.
        let switch = match (self.content.switchable(), self.content.raw) {
            (false, _) => String::new(),
            (true, false) => "   r raw".to_string(),
            (true, true) => format!(
                "   r {}",
                self.content.format.label().unwrap_or("formatted")
            ),
        };
        let footer = if lines.len() > rows {
            let (first, last, total) = (scroll + 1, (scroll + rows).min(lines.len()), lines.len());
            fitting(
                &[
                    format!(" {first}–{last} of {total}   j/k ^d/^u g/G{switch}   q close "),
                    format!(" {first}–{last} of {total}   j/k{switch}   q close "),
                    format!(" {first}–{last} of {total} "),
                ],
                popup.width.saturating_sub(2),
            )
        } else {
            fitting(
                &[
                    format!(" {}   q close ", switch.trim_start()),
                    " q close ".to_string(),
                    " q ".to_string(),
                ],
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
                    // Dim, because it is the same few keys every time.
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
        if self.content.empty {
            Paragraph::new(Line::from(Span::styled(
                "·",
                Style::new().fg(self.theme.null_fg),
            )))
            .render(text, buf);
            return;
        }

        let shown: Vec<Line> = lines.into_iter().skip(scroll).take(rows).collect();
        Paragraph::new(shown).render(text, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn content(value: &str) -> Content {
        Content::new(value, false, &Theme::catppuccin_mocha())
    }

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render(value: &str, area: Rect) -> Vec<String> {
        render_content(&content(value), area, 0)
    }

    fn render_content(content: &Content, area: Rect, scroll: usize) -> Vec<String> {
        let theme = Theme::catppuccin_mocha();
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            content,
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
            text(&wrap(&plain("the quick brown fox"), 10)),
            ["the quick ", "brown fox"],
            "and not mid-word"
        );
    }

    #[test]
    fn a_word_with_nowhere_to_break_is_cut_on_the_width() {
        // A URL or a base64 blob has no space in it; the alternative to
        // cutting is a line wider than the window.
        assert_eq!(text(&wrap(&plain("aaaaaaaa"), 3)), ["aaa", "aaa", "aa"]);
        assert_eq!(text(&wrap(&plain("hi aaaaaa"), 3)), ["hi ", "aaa", "aaa"]);
    }

    #[test]
    fn wrapping_keeps_the_values_own_lines_and_indentation() {
        assert_eq!(
            text(&wrap(&plain("{\n  \"a\": 1\n}"), 40)),
            ["{", "  \"a\": 1", "}"]
        );
        assert_eq!(
            text(&wrap(&plain("a\n\nb"), 40)),
            ["a", "", "b"],
            "a blank line survives"
        );
        assert_eq!(
            text(&wrap(&plain(""), 40)),
            [""],
            "and an empty value is one line"
        );
    }

    #[test]
    fn wrapping_counts_characters_not_bytes() {
        assert_eq!(text(&wrap(&plain("éàü"), 2)), ["éà", "ü"]);
    }

    /// A continuation that started back at the left edge would read as a new
    /// key rather than as more of the one above it.
    #[test]
    fn a_wrapped_line_picks_its_indentation_back_up() {
        let deep = plain("    aaa bbb ccc ddd");
        assert_eq!(text(&wrap(&deep, 12)), ["    aaa bbb ", "    ccc ddd"]);
    }

    #[test]
    fn wrapping_keeps_the_styling_of_what_it_splits() {
        let line = Line::from(vec![
            Span::styled("key ", Style::new().fg(Color::Red)),
            Span::styled("value words here", Style::new().fg(Color::Blue)),
        ]);
        let wrapped = wrap(&[line], 10);
        assert_eq!(text(&wrapped), ["key value ", "words here"]);
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::Blue));
        assert_eq!(wrapped[1].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn the_window_sizes_to_the_value_but_not_past_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let (small, _) = window(&plain("short"), " note ", area);
        assert_eq!(small.width, MIN_WIDTH, "a short value still gets a window");
        assert_eq!(small.height, MIN_HEIGHT);

        let (big, lines) = window(&plain(&"x".repeat(10_000)), " note ", area);
        assert!(big.width <= 72, "nine tenths of the width at most: {big:?}");
        assert!(big.height <= 19, "four fifths of the height: {big:?}");
        assert!(lines.len() > text_rows(big), "so it has to scroll");
    }

    /// A window narrower than its own title would be a border that cannot say
    /// what it is showing.
    #[test]
    fn the_window_is_never_narrower_than_what_it_has_to_say() {
        let content = content("hi");
        let title = title("a-long-column-name", "str", &content);
        let (popup, _) = window(&content.lines, &title, Rect::new(0, 0, 80, 24));
        assert!(
            popup.width as usize >= title.chars().count() + 2,
            "{popup:?} for {title:?}"
        );
    }

    #[test]
    fn the_window_is_centred() {
        let area = Rect::new(0, 0, 80, 24);
        let (popup, _) = window(&plain("hello"), " note ", area);
        assert_eq!(popup.x, (area.width - popup.width) / 2);
        assert_eq!(popup.y, (area.height - popup.height) / 2);
    }

    #[test]
    fn a_terminal_smaller_than_the_window_is_not_overflowed() {
        let area = Rect::new(0, 0, 10, 3);
        let (popup, _) = window(&plain("a value that will not fit"), " note ", area);
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
        let ladder = titles("note", "str", &content("hello world"));
        assert_eq!(fitting(&ladder, 60), ladder[0]);
        assert_eq!(fitting(&ladder, 23), ladder[1]);
        assert_eq!(fitting(&ladder, 10), ladder[2]);
        assert_eq!(fitting(&ladder, 2), ladder[2], "something has to be drawn");
    }

    #[test]
    fn scrolling_moves_the_value_and_the_footer_says_where_it_is() {
        let value: String = (1..=40).map(|n| format!("line{n}\n")).collect();
        let content = content(&value);
        let area = Rect::new(0, 0, 60, 14);
        let (popup, lines) = window(&content.lines, &title("note", "str", &content), area);
        let rows = text_rows(popup);
        let bottom = (popup.y + popup.height - 1) as usize;

        let top = render(&value, area);
        assert!(top.iter().any(|l| l.contains("line1 ")), "{top:?}");

        let down = render_content(&content, area, 5);
        assert!(!down.iter().any(|l| l.contains("line1 ")), "{down:?}");
        assert!(down.iter().any(|l| l.contains("line6")), "{down:?}");
        assert!(
            down[bottom].contains(&format!("6–{} of {}", 5 + rows, lines.len())),
            "{}",
            down[bottom]
        );

        // Past the end, the last line sits at the bottom rather than the value
        // scrolling out of sight.
        let end = render_content(&content, area, 9_999);
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

    #[test]
    fn a_json_cell_is_shown_as_a_document() {
        let content = content(r#"{"id":4821,"tags":["a","b"]}"#);
        assert_eq!(content.format, Format::Json);
        assert_eq!(
            text(&content.lines),
            [
                "{",
                "  \"id\": 4821,",
                "  \"tags\": [",
                "    \"a\",",
                "    \"b\"",
                "  ]",
                "}"
            ]
        );
        assert_eq!(
            content.chars, 28,
            "the size is the cell's, not the formatting's"
        );
    }

    #[test]
    fn the_title_says_which_format_it_recognised_and_when_it_is_showing_bytes() {
        let value = r#"{"id":4821}"#;
        let theme = Theme::catppuccin_mocha();

        let formatted = Content::new(value, false, &theme);
        assert!(title("note", "str", &formatted).contains("json,"));

        // The way back has to be visible, or the file's own line breaks
        // cannot be told from the ones plv added.
        let raw = Content::new(value, true, &theme);
        assert_eq!(raw.format, Format::Json, "still known for what it is");
        assert_eq!(text(&raw.lines), [value], "but shown as it is written");
        assert!(title("note", "str", &raw).contains("json, raw"));
    }

    #[test]
    fn the_footer_offers_the_way_back_only_where_there_is_one() {
        let area = Rect::new(0, 0, 60, 12);
        let plain = render("hello world", area);
        assert!(
            !plain.iter().any(|line| line.contains("r raw")),
            "nothing to switch to: {plain:?}"
        );

        let json = render(r#"{"id":4821}"#, area);
        assert!(json.iter().any(|line| line.contains("r raw")), "{json:?}");

        let raw = render_content(
            &Content::new(r#"{"id":4821}"#, true, &Theme::catppuccin_mocha()),
            area,
            0,
        );
        assert!(raw.iter().any(|line| line.contains("r json")), "{raw:?}");
    }

    #[test]
    fn a_markup_cell_is_laid_out_and_named_for_what_it_is() {
        let html = content(r#"<div class="note"><p>Hi <b>there</b></p><br></div>"#);
        assert_eq!(html.format, Format::Markup(markup::Flavour::Html));
        assert_eq!(
            text(&html.lines),
            [
                r#"<div class="note">"#,
                "  <p>Hi <b>there</b></p>",
                "  <br>",
                "</div>",
            ]
        );
        assert!(title("note", "str", &html).contains("html,"));

        // Closed the way XML asks, and it is XML.
        let xml = content("<note><to>you</to></note>");
        assert_eq!(xml.format, Format::Markup(markup::Flavour::Xml));
        assert!(title("note", "str", &xml).contains("xml,"));
        assert!(xml.switchable(), "and `r` gets back to the bytes");
    }

    /// Prose with an angle bracket in it is prose. A format is claimed only
    /// when the whole value parses as one.
    #[test]
    fn a_value_that_is_not_a_document_is_left_alone() {
        let content = content("just a note, {not} json, and 3 < 4 <p>");
        assert_eq!(content.format, Format::Text);
        assert!(!content.switchable(), "and offers no way back");
        assert_eq!(
            text(&content.lines),
            ["just a note, {not} json, and 3 < 4 <p>"]
        );
    }
}
