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
use regex::Regex;

use super::{Theme, json, markdown, markup, syntax};

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
    Markdown,
}

impl Format {
    /// What the title calls it, or `None` where there is nothing to say.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Format::Text => None,
            Format::Json => Some("json"),
            Format::Markup(flavour) => Some(flavour.label()),
            Format::Markdown => Some("markdown"),
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
        // Markdown is asked last, and has to be: the other two are parses
        // that a value either passes or does not, and this one is a judgement
        // about whether there is structure worth drawing. A JSON document
        // full of `*` would otherwise be claimed by the loosest test.
        let (format, document) = match json::reindent(value) {
            Some(document) => (Format::Json, Some(document)),
            None => match markup::reindent(value) {
                Some((document, flavour)) => (Format::Markup(flavour), Some(document)),
                None => match markdown::format(value) {
                    Some(document) => (Format::Markdown, Some(document)),
                    None => (Format::Text, None),
                },
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
    wrapped(lines, width).0
}

/// Where a wrapped line came from: which of the value's own lines, and how
/// far into it this piece begins.
///
/// It exists so a search can be run on the value and drawn on the screen.
/// `indent` counts the hanging-indent spaces the wrap *added* at the front of
/// a continuation — they are plv's, not the value's, so a character at
/// wrapped position `indent + n` is character `start + n` of the source line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Origin {
    pub line: usize,
    pub start: usize,
    pub indent: usize,
}

/// [`wrap`], and where each line it produced came from.
pub fn wrapped(lines: &[Line<'static>], width: u16) -> (Vec<Line<'static>>, Vec<Origin>) {
    let mut wrapper = Wrapper {
        width: (width as usize).max(1),
        indent: 0,
        out: Vec::new(),
        origins: Vec::new(),
        current: Vec::new(),
        have: 0,
        source: 0,
        consumed: 0,
        began: 0,
        padding: 0,
    };
    for (nth, line) in lines.iter().enumerate() {
        wrapper.line(nth, line);
    }
    (wrapper.out, wrapper.origins)
}

struct Wrapper {
    width: usize,
    /// What a continuation of the current line starts with.
    indent: usize,
    out: Vec<Line<'static>>,
    origins: Vec<Origin>,
    current: Vec<Span<'static>>,
    have: usize,
    /// Which of the value's lines is being wrapped.
    source: usize,
    /// Characters of it emitted so far — the value's own, so the hanging
    /// indent the wrap adds is deliberately not counted.
    consumed: usize,
    /// What `consumed` was when the line being built started.
    began: usize,
    /// Hanging-indent characters at the front of the line being built.
    padding: usize,
}

impl Wrapper {
    fn line(&mut self, nth: usize, line: &Line<'static>) {
        self.source = nth;
        self.consumed = 0;
        self.began = 0;
        self.padding = 0;
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
        self.emit();
    }

    /// Close the line being built, noting where in the value it came from.
    fn emit(&mut self) {
        self.out.push(Line::from(std::mem::take(&mut self.current)));
        self.origins.push(Origin {
            line: self.source,
            start: self.began,
            indent: self.padding,
        });
        self.have = 0;
        self.began = self.consumed;
        self.padding = 0;
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
        let chars = text.chars().count();
        self.have += chars;
        self.consumed += chars;
        self.current.push(Span::styled(text, style));
    }

    fn wrap_here(&mut self) {
        self.emit();
        if self.indent > 0 {
            // Padding, not value: it takes room on the line but stands for no
            // character of the source, so `consumed` must not move.
            self.have += self.indent;
            self.padding = self.indent;
            self.current
                .push(Span::styled(" ".repeat(self.indent), Style::new()));
        }
    }
}

/// A piece of one match, as a range of a wrapped line — what actually gets
/// drawn in the match colours.
///
/// **The pattern is matched against the value, not against the screen.** The
/// wrap breaks lines at spaces, so searching what is drawn would mean `line
/// 137` is found or not found depending on how wide the window happens to be,
/// and a resize would silently change the answer. So the match is found in
/// the value's own line and then *placed* on the wrapped lines that draw it.
///
/// Which is why a hit is a piece rather than the whole: a run broken across a
/// wrap is drawn in two places and is still one match. `nth` says which match
/// this is a piece of, so the footer can count matches while the renderer
/// colours fragments, and `n` steps by the first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hit {
    pub nth: usize,
    pub line: usize,
    /// Characters, not bytes: it is what the wrap counts and where a span is
    /// cut, and the two must agree or a highlight lands beside its word.
    pub start: usize,
    pub end: usize,
}

/// The characters of a line, with its styling dropped — what a pattern is
/// matched against.
fn plain_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|span| &*span.content).collect()
}

/// One match, in the coordinates of the value's own lines.
struct Match {
    line: usize,
    start: usize,
    end: usize,
}

/// Every match of `regex` in `source` — the value's own lines — placed on the
/// `wrapped` lines that draw them, which `origins` maps back.
///
/// An empty match is skipped rather than counted. A pattern like `x*` matches
/// at every position, and a search reporting `1/4212` that never moves has
/// answered nothing.
pub fn find(
    source: &[Line<'static>],
    wrapped: &[Line<'static>],
    origins: &[Origin],
    regex: &Regex,
) -> Vec<Hit> {
    let mut found = Vec::new();
    for (line, text) in source.iter().enumerate() {
        let text = plain_text(text);
        // One pass per line converting byte offsets to character ones, rather
        // than one per match: a line of CJK or emoji would otherwise be walked
        // from the start again for every hit on it.
        let mut chars = text.char_indices().enumerate().peekable();
        let mut char_at = |byte: usize| {
            while let Some(&(nth, (at, _))) = chars.peek() {
                if at >= byte {
                    return nth;
                }
                chars.next();
            }
            text.chars().count()
        };
        for one in regex.find_iter(&text) {
            if one.start() == one.end() {
                continue;
            }
            found.push(Match {
                line,
                start: char_at(one.start()),
                end: char_at(one.end()),
            });
        }
    }
    place(&found, wrapped, origins)
}

/// Cut each match into the pieces of it that fall on wrapped lines.
///
/// A match that the wrap broke lands on two lines and keeps one `nth`, so it
/// is highlighted in both places and counted once.
fn place(found: &[Match], wrapped: &[Line<'static>], origins: &[Origin]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (nth, one) in found.iter().enumerate() {
        for (line, origin) in origins.iter().enumerate() {
            if origin.line != one.line {
                continue;
            }
            // What this wrapped line holds of its source line, as a range of
            // that source line.
            let held = line_chars(&wrapped[line]).saturating_sub(origin.indent);
            let (from, to) = (origin.start, origin.start + held);
            let (start, end) = (one.start.max(from), one.end.min(to));
            if start >= end {
                continue;
            }
            hits.push(Hit {
                nth,
                line,
                start: origin.indent + (start - from),
                end: origin.indent + (end - from),
            });
        }
    }
    hits
}

/// How many matches `hits` are pieces of.
pub fn matches(hits: &[Hit]) -> usize {
    hits.last().map_or(0, |hit| hit.nth + 1)
}

/// The wrapped line each match begins on, in order — what `n` steps between.
/// A match broken by the wrap is one entry, at the line it starts on.
pub fn match_lines(hits: &[Hit]) -> Vec<usize> {
    let mut lines: Vec<usize> = Vec::with_capacity(matches(hits));
    for hit in hits {
        if hit.nth == lines.len() {
            lines.push(hit.line);
        }
    }
    lines
}

/// `line` with the parts of it that matched drawn as matches.
///
/// The value's own colouring is kept underneath — a highlight says *this is
/// what you searched for*, not *this is a different kind of thing* — so only
/// the background and foreground are replaced and the weight and slant a
/// document gave a run survive being found.
fn highlighted(
    line: &Line<'static>,
    hits: &[Hit],
    current: Option<usize>,
    theme: &Theme,
) -> Line<'static> {
    if hits.is_empty() {
        return line.clone();
    }
    let found = Style::new().bg(theme.match_bg).fg(theme.match_fg);
    let here = Style::new()
        .bg(theme.cursor_bg)
        .fg(theme.cursor_fg)
        .add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut at = 0;
    for span in &line.spans {
        // Cut this span wherever a match starts or ends inside it, and style
        // each piece by whether it is in one.
        let mut cuts: Vec<usize> = Vec::new();
        let end = at + span.content.chars().count();
        for hit in hits {
            for edge in [hit.start, hit.end] {
                if edge > at && edge < end {
                    cuts.push(edge);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();

        let chars: Vec<char> = span.content.chars().collect();
        let mut from = at;
        for to in cuts.into_iter().chain(std::iter::once(end)) {
            let text: String = chars[from - at..to - at].iter().collect();
            let hit = hits.iter().find(|h| h.start <= from && to <= h.end);
            let style = match hit {
                Some(hit) if current == Some(hit.nth) => span.style.patch(here),
                Some(_) => span.style.patch(found),
                None => span.style,
            };
            spans.push(Span::styled(text, style));
            from = to;
        }
        at = end;
    }
    Line::from(spans)
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
pub fn window(lines: &[Line<'static>], title: &str, area: Rect) -> Layout {
    let longest = lines.iter().map(line_chars).max().unwrap_or(0);

    let widest = fraction(area.width, 9, 10).clamp(MIN_WIDTH.min(area.width), area.width);
    let width = (longest + CHROME as usize)
        .max(title.chars().count() + 2)
        .min(widest as usize)
        .max(MIN_WIDTH.min(area.width) as usize) as u16;

    let (lines, origins) = wrapped(lines, width.saturating_sub(CHROME));

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
    Layout {
        rect,
        lines,
        origins,
    }
}

/// Where the window sits and what goes in it — one answer, so the renderer,
/// the scroll clamp and the search cannot disagree about how the value broke.
pub struct Layout {
    pub rect: Rect,
    pub lines: Vec<Line<'static>>,
    pub origins: Vec<Origin>,
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
    /// What the window's own `/` found, in reading order, and which of them
    /// `n` last landed on. Worked out by the app layer, which is where the
    /// pattern lives and where `n` moves.
    pub hits: &'a [Hit],
    pub current: Option<usize>,
    /// The pattern itself, for the footer to say what is being stepped
    /// through — a count with no word beside it does not say what was found.
    pub pattern: Option<&'a str>,
    pub theme: &'a Theme,
}

impl Widget for CellWindow<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let ladder = titles(self.name, self.dtype, self.content);
        let Layout {
            rect: popup, lines, ..
        } = window(&self.content.lines, &ladder[0], area);
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
        // What `/` found, said in the footer beside the position: which match
        // of how many, and the pattern they are matches of. A bare `3/12`
        // says how far through something the reader is without saying through
        // what — and by the time it matters they have scrolled away from the
        // prompt they typed it at.
        //
        // A pattern with nothing to show says so. Falling silent would leave
        // "found nothing" and "never searched" looking the same, and the
        // second is the one the reader will assume.
        let found = match (self.pattern, matches(self.hits)) {
            (None, _) => String::new(),
            (Some(pattern), 0) => format!("   /{pattern} none"),
            (Some(pattern), total) => format!(
                "   /{pattern} {}/{total}",
                self.current.map_or(0, |at| at + 1)
            ),
        };
        // `n/N` is offered only where there is more than one match to step
        // between, for the reason a lone sort key is drawn without its
        // priority number: a next among one thing is the thing you are on.
        let steps = if matches(self.hits) > 1 { " n/N" } else { "" };
        let footer = if lines.len() > rows {
            let (first, last, total) = (scroll + 1, (scroll + rows).min(lines.len()), lines.len());
            fitting(
                &[
                    format!(
                        " {first}–{last} of {total}{found}   j/k ^d/^u g/G   /{steps}{switch}   q close "
                    ),
                    format!(
                        " {first}–{last} of {total}{found}   j/k   /{steps}{switch}   q close "
                    ),
                    format!(" {first}–{last} of {total}{found}   j/k   /   q close "),
                    format!(" {first}–{last} of {total}{found} "),
                    format!(" {first}–{last} of {total} "),
                ],
                popup.width.saturating_sub(2),
            )
        } else {
            // Nothing to scroll, so `/` is the only movement there is — and
            // still worth offering, since a value that fits the window can
            // still be one nobody wants to read all of.
            let said = match found.trim_start() {
                "" => String::new(),
                text => format!("{text}   "),
            };
            fitting(
                &[
                    format!(" {said}/{steps}{switch}   q close "),
                    format!(" {said}/   q close "),
                    " /   q close ".to_string(),
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

        // Only the lines on screen are highlighted: the hits are already
        // known for the whole value, and restyling the ones nobody is looking
        // at would be paying for the document on every keypress, which is the
        // cost `Content` exists to have paid once.
        let shown: Vec<Line> = lines
            .into_iter()
            .enumerate()
            .skip(scroll)
            .take(rows)
            .map(|(nth, line)| {
                let on_line: Vec<Hit> = self
                    .hits
                    .iter()
                    .copied()
                    .filter(|hit| hit.line == nth)
                    .collect();
                highlighted(&line, &on_line, self.current, self.theme)
            })
            .collect();
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
            hits: &[],
            current: None,
            pattern: None,
            theme: &theme,
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    // ── Search ───────────────────────────────────────────────────────────

    fn hits_in(value: &str, pattern: &str, width: u16) -> Vec<Hit> {
        let content = content(value);
        let (lines, origins) = wrapped(&content.lines, width);
        find(
            &content.lines,
            &lines,
            &origins,
            &Regex::new(pattern).unwrap(),
        )
    }

    #[test]
    fn a_match_is_found_where_the_wrap_put_it() {
        let hits = hits_in("alpha beta gamma delta", "delta", 12);
        assert_eq!(hits.len(), 1);
        // Not line 0: the value wrapped, and a match is a place on screen.
        assert!(hits[0].line > 0, "{hits:?}");
    }

    #[test]
    fn every_match_is_found_in_reading_order() {
        let hits = hits_in("one two one two one", "one", 80);
        assert_eq!(hits.len(), 3);
        assert!(hits.windows(2).all(|w| w[0].start < w[1].start));
    }

    /// `x*` matches at every position between characters. Counting those
    /// would report hundreds of matches that `n` cannot move between.
    #[test]
    fn an_empty_match_is_not_a_match() {
        assert!(hits_in("abc", "x*", 80).is_empty());
    }

    /// The offsets are what the highlight is cut at, so they have to be in
    /// the same units the spans are counted in.
    #[test]
    fn offsets_are_characters_and_not_bytes() {
        let hits = hits_in("café note", "note", 80);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].end), (5, 9));
    }

    /// **The width must not change the answer.** The pattern is matched
    /// against the value, so a run the wrap broke is still found — drawn in
    /// two pieces, counted as one match. Searching the screen instead would
    /// mean a resize silently turned a match into nothing.
    #[test]
    fn a_match_broken_by_the_wrap_is_still_one_match() {
        // Wrapped at 12, "gamma" cannot share a line with "alpha beta".
        let narrow = hits_in("alpha beta gamma", "beta gamma", 12);
        let wide = hits_in("alpha beta gamma", "beta gamma", 80);
        assert_eq!(matches(&narrow), 1, "found across the break: {narrow:?}");
        assert_eq!(matches(&wide), 1);
        assert_eq!(narrow.len(), 2, "and drawn in both places");
        assert_eq!(wide.len(), 1);
        assert!(narrow.iter().all(|hit| hit.nth == 0));
    }

    /// The same pattern over the same value finds the same number of matches
    /// however the window is sized. This is the property the whole placement
    /// dance exists for.
    #[test]
    fn the_window_width_does_not_change_what_was_found() {
        let value: String = (1..=60).map(|n| format!("line {n}. ")).collect();
        let at = |width| matches(&hits_in(&value, "line 1[0-9]", width));
        let wide = at(200);
        assert_eq!(wide, 10, "line 10 through line 19");
        for width in [14, 20, 33, 47, 80, 120] {
            assert_eq!(at(width), wide, "at width {width}");
        }
    }

    /// A hanging indent is plv's own, so it must not be counted as characters
    /// of the value — a highlight would land that many columns to the left.
    #[test]
    fn a_hanging_indent_does_not_shift_the_highlight() {
        let value = "    keep alpha beta gamma delta epsilon";
        let hits = hits_in(value, "epsilon", 20);
        assert_eq!(matches(&hits), 1);
        let content = content(value);
        let (lines, _) = wrapped(&content.lines, 20);
        let hit = hits[0];
        let drawn: String = text(&lines)[hit.line].chars().collect();
        let at: String = drawn
            .chars()
            .skip(hit.start)
            .take(hit.end - hit.start)
            .collect();
        assert_eq!(at, "epsilon", "in {drawn:?}");
    }

    #[test]
    fn a_match_is_drawn_in_the_match_colours() {
        let theme = Theme::catppuccin_mocha();
        let content = content("alpha beta gamma");
        let area = Rect::new(0, 0, 40, 8);
        let hits = hits_in("alpha beta gamma", "beta", 36);
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            content: &content,
            scroll: 0,
            hits: &hits,
            current: Some(0),
            pattern: Some("beta"),
            theme: &theme,
        }
        .render(area, &mut buf);

        let found = (0..area.height).any(|y| {
            (0..area.width).any(|x| {
                buf[(x, y)].symbol() == "b" && buf[(x, y)].style().bg == Some(theme.cursor_bg)
            })
        });
        assert!(found, "the current match is drawn as the current match");
    }

    /// The window's own colouring is what a document *is*; a highlight says
    /// only that this is what was asked for. Losing the first to show the
    /// second would make a found key stop looking like a key.
    #[test]
    fn a_highlight_keeps_the_styling_underneath_it() {
        let theme = Theme::catppuccin_mocha();
        let styled = Line::from(vec![Span::styled(
            "deploy".to_string(),
            Style::new().add_modifier(Modifier::BOLD),
        )]);
        let source = std::slice::from_ref(&styled);
        let (lines, origins) = wrapped(source, 40);
        let hits = find(source, &lines, &origins, &Regex::new("epl").unwrap());
        let out = highlighted(&lines[0], &hits, Some(0), &theme);

        let inside = out
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "epl")
            .expect("the match is a span of its own");
        assert!(inside.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(inside.style.bg, Some(theme.cursor_bg));
    }

    #[test]
    fn the_footer_says_which_match_of_how_many() {
        let theme = Theme::catppuccin_mocha();
        let content = content("one two one two one");
        let area = Rect::new(0, 0, 60, 8);
        let hits = hits_in("one two one two one", "one", 56);
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            content: &content,
            scroll: 0,
            hits: &hits,
            current: Some(1),
            pattern: Some("one"),
            theme: &theme,
        }
        .render(area, &mut buf);
        let drawn: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(
            drawn.iter().any(|line| line.contains("/one 2/3")),
            "{drawn:#?}"
        );
    }

    /// "Found nothing" and "never searched" must not look the same, or the
    /// reader will read the first as the second and go on scrolling.
    #[test]
    fn a_pattern_that_matched_nothing_says_so() {
        let theme = Theme::catppuccin_mocha();
        let content = content("alpha beta");
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            content: &content,
            scroll: 0,
            hits: &[],
            current: None,
            pattern: Some("zeta"),
            theme: &theme,
        }
        .render(area, &mut buf);
        let drawn: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(
            drawn.iter().any(|line| line.contains("/zeta none")),
            "{drawn:#?}"
        );
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
        let Layout { rect: small, .. } = window(&plain("short"), " note ", area);
        assert_eq!(small.width, MIN_WIDTH, "a short value still gets a window");
        assert_eq!(small.height, MIN_HEIGHT);

        let Layout {
            rect: big, lines, ..
        } = window(&plain(&"x".repeat(10_000)), " note ", area);
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
        let Layout { rect: popup, .. } = window(&content.lines, &title, Rect::new(0, 0, 80, 24));
        assert!(
            popup.width as usize >= title.chars().count() + 2,
            "{popup:?} for {title:?}"
        );
    }

    #[test]
    fn the_window_is_centred() {
        let area = Rect::new(0, 0, 80, 24);
        let Layout { rect: popup, .. } = window(&plain("hello"), " note ", area);
        assert_eq!(popup.x, (area.width - popup.width) / 2);
        assert_eq!(popup.y, (area.height - popup.height) / 2);
    }

    #[test]
    fn a_terminal_smaller_than_the_window_is_not_overflowed() {
        let area = Rect::new(0, 0, 10, 3);
        let Layout { rect: popup, .. } =
            window(&plain("a value that will not fit"), " note ", area);
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
        let Layout {
            rect: popup, lines, ..
        } = window(&content.lines, &title("note", "str", &content), area);
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

    #[test]
    fn a_markdown_cell_is_marked_up_without_being_rewritten() {
        let note = "## Deploy\n\n- rollback with `plv down`\n- **check** the queue";
        let content = content(note);
        assert_eq!(content.format, Format::Markdown);
        assert!(title("note", "str", &content).contains("markdown,"));
        assert_eq!(
            text(&content.lines).join("\n"),
            note,
            "every character the cell holds is still on screen"
        );
    }

    /// The text is the cell's, so what a reader actually gets is the
    /// *styling* — which means it has to be checked, not assumed.
    #[test]
    fn a_heading_is_drawn_bold_and_its_hashes_are_not() {
        let theme = Theme::catppuccin_mocha();
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        CellWindow {
            name: "note",
            dtype: "str",
            content: &content("## Deploy\n- a\n- b"),
            scroll: 0,
            hits: &[],
            current: None,
            pattern: None,
            theme: &theme,
        }
        .render(area, &mut buf);

        let row = (0..area.height)
            .find(|&y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("## Deploy")
            })
            .expect("the heading is on screen");
        let at = |needle: char| {
            (0..area.width)
                .find(|&x| buf[(x, row)].symbol() == needle.to_string())
                .expect("found")
        };
        let hash = at('#');
        let word = at('D');
        assert!(
            buf[(word, row)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "the heading's words are bold"
        );
        assert!(
            !buf[(hash, row)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "and its marker recedes rather than joining in"
        );
        assert_eq!(buf[(hash, row)].style().fg, Some(theme.syntax_punct));
    }

    /// The loosest test is asked last, or it would claim what the others
    /// would have parsed.
    #[test]
    fn a_document_full_of_markers_is_still_the_document_it_is() {
        let json = r##"{"note":"* not a bullet","body":"# not a heading"}"##;
        assert_eq!(content(json).format, Format::Json);
        let html = "<ul><li>* not a bullet</li><li># not a heading</li></ul>";
        assert_eq!(
            content(html).format,
            Format::Markup(markup::Flavour::Xml),
            "and markup is asked before markdown too"
        );
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
