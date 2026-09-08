//! Re-indenting XML and HTML, without rebuilding it.
//!
//! The same argument as [`super::json`], and the same promise. A 4KB fragment
//! written on one line is unreadable, and what makes it unreadable is that the
//! structure — which *is* the content — has nowhere to show. So the tags are
//! laid out and the original bytes are re-emitted: attribute quoting, entities,
//! whitespace inside prose and the document's own idea of how to write a value
//! all survive. The only things added are line breaks and indentation.
//!
//! **Well-formed or nothing.** Every tag has to close, in the right order, and
//! the walk has to reach the end of the value. Real HTML is often not like
//! that — an unclosed `<p>`, an `<li>` left hanging — and those stay text.
//! That is deliberate: a half-parsed document drawn as a tree is a claim about
//! where things nest, and getting that wrong is worse than not indenting at
//! all. HTML's void elements are the one exception, because `<br>` closing
//! itself is the rule rather than a mistake, and their list is finite.
//!
//! An element whose children are *all* elements is broken out, one to a line.
//! One with any text in it is left on a single line, because breaking prose
//! apart to show its structure would be showing structure that is not there —
//! the window wraps it like any other long line.

use super::syntax::{Kind, Piece};

/// Which of the two this turned out to be.
///
/// One parser, because the difference that matters here is small: a document
/// that needed HTML's rules to parse is HTML, and one that did not is XML.
/// Said in the title, so the reader knows which of the two they are looking
/// at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavour {
    Xml,
    Html,
}

impl Flavour {
    pub fn label(self) -> &'static str {
        match self {
            Flavour::Xml => "xml",
            Flavour::Html => "html",
        }
    }
}

/// Elements HTML closes for you. An `<img>` with no `</img>` is correct HTML
/// and malformed XML, so meeting one is what tells the two apart.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is not markup. A `<` inside `<script>` is a
/// less-than sign, and reading it as a tag would fail on the first one.
const RAW_TEXT: &[&str] = &["script", "style"];

/// Spaces per level.
const INDENT: usize = 2;
/// Nesting past this is refused rather than recursed into.
const MAX_DEPTH: usize = 64;

/// The value re-indented, and which flavour it was — or `None` when it is not
/// a well-formed document with at least one element in it.
///
/// The last part is what keeps prose out: a cell of text is not markup however
/// many `<` it happens to contain, and a document with no element has nothing
/// to lay out.
pub fn reindent(value: &str) -> Option<(Vec<Vec<Piece>>, Flavour)> {
    let mut parser = Parser {
        source: value.as_bytes(),
        at: 0,
        html: false,
        depth: 0,
    };
    let nodes = parser.nodes(None)?;
    if parser.at != value.len() || !nodes.iter().any(|node| matches!(node, Node::Element(_))) {
        return None;
    }

    let mut out = Vec::new();
    render(&nodes, 0, &mut out);
    Some((
        out,
        match parser.html {
            true => Flavour::Html,
            false => Flavour::Xml,
        },
    ))
}

enum Node {
    Element(Element),
    /// Ordinary text between tags, exactly as written.
    Text(String),
    /// A comment, a doctype, a processing instruction, a CDATA section: kept
    /// whole, since none of them has structure worth laying out.
    Aside(String),
}

struct Element {
    /// The opening tag, already broken into its coloured runs.
    open: Vec<Piece>,
    children: Vec<Node>,
    /// The closing tag, absent for one that closed itself.
    close: Option<Vec<Piece>>,
}

/// Whether these children are structure rather than prose.
///
/// Text in an element is the thing being read, and putting each run of it on
/// its own line would be inventing paragraph breaks the document does not
/// have. Whitespace alone is not prose: between elements it is the
/// formatter's, and this is the formatter.
fn breaks(children: &[Node]) -> bool {
    children
        .iter()
        .any(|child| matches!(child, Node::Element(_)))
        && !has_prose(children)
}

fn has_prose(nodes: &[Node]) -> bool {
    nodes
        .iter()
        .any(|node| matches!(node, Node::Text(text) if !text.trim().is_empty()))
}

/// A run of nodes: one to a line where they are structure, all on one line
/// where any of them is prose.
fn render(nodes: &[Node], indent: usize, out: &mut Vec<Vec<Piece>>) {
    if has_prose(nodes) {
        let mut line = opening(indent);
        for node in nodes {
            inline(node, &mut line);
        }
        out.push(line);
        return;
    }
    for node in nodes {
        // Whitespace between block elements is what indentation replaces.
        if matches!(node, Node::Text(text) if text.trim().is_empty()) {
            continue;
        }
        block(node, indent, out);
    }
}

fn block(node: &Node, indent: usize, out: &mut Vec<Vec<Piece>>) {
    let mut line = opening(indent);
    match node {
        Node::Text(text) => line.push(Piece::new(Kind::Text, text.clone())),
        Node::Aside(text) => line.push(Piece::new(Kind::Punct, text.clone())),
        Node::Element(element) if breaks(&element.children) => {
            line.extend(element.open.iter().cloned());
            out.push(line);
            render(&element.children, indent + 1, out);
            if let Some(close) = &element.close {
                let mut line = opening(indent);
                line.extend(close.iter().cloned());
                out.push(line);
            }
            return;
        }
        Node::Element(element) => {
            line.extend(element.open.iter().cloned());
            for child in &element.children {
                inline(child, &mut line);
            }
            if let Some(close) = &element.close {
                line.extend(close.iter().cloned());
            }
        }
    }
    out.push(line);
}

/// Everything of this node, appended to the line already being written.
fn inline(node: &Node, line: &mut Vec<Piece>) {
    match node {
        Node::Text(text) => line.push(Piece::new(Kind::Text, text.clone())),
        Node::Aside(text) => line.push(Piece::new(Kind::Punct, text.clone())),
        Node::Element(element) => {
            line.extend(element.open.iter().cloned());
            for child in &element.children {
                inline(child, line);
            }
            if let Some(close) = &element.close {
                line.extend(close.iter().cloned());
            }
        }
    }
}

fn opening(indent: usize) -> Vec<Piece> {
    match indent {
        0 => Vec::new(),
        _ => vec![Piece::new(Kind::Punct, " ".repeat(indent * INDENT))],
    }
}

struct Parser<'a> {
    source: &'a [u8],
    at: usize,
    /// Something needed HTML's rules to parse.
    html: bool,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.source.get(self.at).copied()
    }

    fn starts_with(&self, text: &str) -> bool {
        self.source[self.at..].starts_with(text.as_bytes())
    }

    fn text(&self, span: std::ops::Range<usize>) -> String {
        String::from_utf8_lossy(&self.source[span]).into_owned()
    }

    /// Nodes until the close tag for `until`, or until the end of the value
    /// when there is none.
    fn nodes(&mut self, until: Option<&str>) -> Option<Vec<Node>> {
        let mut nodes = Vec::new();
        loop {
            if self.at >= self.source.len() {
                // A document that ends inside an element is not well formed,
                // whatever the first half looked like.
                return until.is_none().then_some(nodes);
            }
            if self.starts_with("</") {
                return until.is_some().then_some(nodes);
            }
            if self.starts_with("<!--") {
                nodes.push(self.until("-->")?);
            } else if self.starts_with("<![CDATA[") {
                nodes.push(self.until("]]>")?);
            } else if self.starts_with("<!") {
                if self.source[self.at..].len() > 9
                    && self.source[self.at + 2..self.at + 9].eq_ignore_ascii_case(b"DOCTYPE")
                {
                    self.html = true;
                }
                nodes.push(self.until(">")?);
            } else if self.starts_with("<?") {
                nodes.push(self.until("?>")?);
            } else if self.peek() == Some(b'<') {
                nodes.push(self.element()?);
            } else {
                nodes.push(self.prose()?);
            }
        }
    }

    /// A run kept whole, up to and including `end`.
    fn until(&mut self, end: &str) -> Option<Node> {
        let start = self.at;
        let at = self.source[start..]
            .windows(end.len())
            .position(|window| window == end.as_bytes())?;
        self.at = start + at + end.len();
        Some(Node::Aside(self.text(start..self.at)))
    }

    /// Text between tags. Never empty, so the walk always advances.
    fn prose(&mut self) -> Option<Node> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte != b'<') {
            self.at += 1;
        }
        (self.at > start).then(|| Node::Text(self.text(start..self.at)))
    }

    fn element(&mut self) -> Option<Node> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let mut open = vec![Piece::new(Kind::Punct, "<")];
        self.at += 1;
        let name = self.name()?;
        open.push(Piece::new(Kind::Name, name.clone()));
        self.attributes(&mut open)?;

        if self.starts_with("/>") {
            self.at += 2;
            open.push(Piece::new(Kind::Punct, "/>"));
            return Some(Node::Element(Element {
                open,
                children: Vec::new(),
                close: None,
            }));
        }
        if self.peek()? != b'>' {
            return None;
        }
        self.at += 1;
        open.push(Piece::new(Kind::Punct, ">"));

        let lower = name.to_ascii_lowercase();
        if VOID.contains(&lower.as_str()) {
            // Closing itself is the rule for these, not a mistake — and it is
            // the rule that says this is HTML.
            self.html = true;
            return Some(Node::Element(Element {
                open,
                children: Vec::new(),
                close: None,
            }));
        }

        let children = if RAW_TEXT.contains(&lower.as_str()) {
            self.html = true;
            let start = self.at;
            let end = self.find_close(&name)?;
            self.at = end;
            match end > start {
                true => vec![Node::Text(self.text(start..end))],
                false => Vec::new(),
            }
        } else {
            self.depth += 1;
            let children = self.nodes(Some(&name))?;
            self.depth -= 1;
            children
        };

        // The matching close tag, and only that one.
        if !self.starts_with("</") {
            return None;
        }
        self.at += 2;
        let closing = self.name()?;
        if !closing.eq_ignore_ascii_case(&name) {
            return None;
        }
        self.space();
        if self.peek()? != b'>' {
            return None;
        }
        self.at += 1;
        let close = vec![
            Piece::new(Kind::Punct, "</"),
            Piece::new(Kind::Name, closing),
            Piece::new(Kind::Punct, ">"),
        ];
        Some(Node::Element(Element {
            open,
            children,
            close: Some(close),
        }))
    }

    /// Where the close tag for a raw-text element begins.
    fn find_close(&self, name: &str) -> Option<usize> {
        let needle = format!("</{name}");
        let bytes = needle.as_bytes();
        self.source[self.at..]
            .windows(bytes.len())
            .position(|window| window.eq_ignore_ascii_case(bytes))
            .map(|at| self.at + at)
    }

    fn name(&mut self) -> Option<String> {
        let start = self.at;
        if !self
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        {
            return None;
        }
        while self.peek().is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
        }) {
            self.at += 1;
        }
        Some(self.text(start..self.at))
    }

    fn space(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    /// Attributes, appended to the opening tag's runs.
    fn attributes(&mut self, open: &mut Vec<Piece>) -> Option<()> {
        loop {
            let before = self.at;
            self.space();
            if matches!(self.peek()?, b'>' | b'/') {
                return Some(());
            }
            if self.at == before {
                // No space before it, so it is not an attribute — and not
                // anything else this understands either.
                return None;
            }
            open.push(Piece::new(Kind::Punct, " "));
            let name = self.name()?;
            open.push(Piece::new(Kind::Attr, name));
            let before = self.at;
            self.space();
            if self.peek()? != b'=' {
                // A bare attribute: `<input disabled>`, which XML does not
                // allow and HTML does.
                self.at = before;
                self.html = true;
                continue;
            }
            self.at += 1;
            open.push(Piece::new(Kind::Punct, "="));
            self.space();
            let value = self.value()?;
            open.push(Piece::new(Kind::Str, value));
        }
    }

    /// An attribute's value, quotes and all, as written.
    fn value(&mut self) -> Option<String> {
        let start = self.at;
        match self.peek()? {
            quote @ (b'"' | b'\'') => {
                self.at += 1;
                while self.peek()? != quote {
                    self.at += 1;
                }
                self.at += 1;
            }
            _ => {
                // Unquoted, which is HTML's alone.
                self.html = true;
                while self
                    .peek()
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'>' | b'/'))
                {
                    self.at += 1;
                }
                if self.at == start {
                    return None;
                }
            }
        }
        Some(self.text(start..self.at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(value: &str) -> Option<Vec<String>> {
        let (document, _) = reindent(value)?;
        Some(
            document
                .iter()
                .map(|line| line.iter().map(|piece| piece.text.as_str()).collect())
                .collect(),
        )
    }

    fn flavour(value: &str) -> Option<Flavour> {
        reindent(value).map(|(_, flavour)| flavour)
    }

    #[test]
    fn elements_are_broken_out_one_to_a_line() {
        assert_eq!(
            shown("<a><b><c/></b></a>").unwrap(),
            ["<a>", "  <b>", "    <c/>", "  </b>", "</a>"]
        );
    }

    /// Breaking prose apart to show its structure would be showing structure
    /// that is not there.
    #[test]
    fn an_element_holding_text_stays_on_one_line() {
        assert_eq!(
            shown("<div><p>Hi <b>there</b></p></div>").unwrap(),
            ["<div>", "  <p>Hi <b>there</b></p>", "</div>"]
        );
    }

    #[test]
    fn attributes_are_kept_exactly_as_written() {
        assert_eq!(
            shown(r#"<a href='/x?a=1&amp;b=2' data-n="3"><b/></a>"#).unwrap(),
            [r#"<a href='/x?a=1&amp;b=2' data-n="3">"#, "  <b/>", "</a>"],
            "quoting style and entities are the document's own"
        );
    }

    #[test]
    fn a_void_element_needs_no_close_and_says_this_is_html() {
        assert_eq!(
            shown("<div><br><img src=x></div>").unwrap(),
            ["<div>", "  <br>", "  <img src=x>", "</div>"]
        );
        assert_eq!(flavour("<div><br></div>"), Some(Flavour::Html));
        assert_eq!(flavour("<div><br/></div>"), Some(Flavour::Xml));
    }

    #[test]
    fn what_needed_htmls_rules_is_html_and_what_did_not_is_xml() {
        assert_eq!(flavour("<note><to>x</to></note>"), Some(Flavour::Xml));
        assert_eq!(flavour("<input disabled>"), Some(Flavour::Html));
        assert_eq!(flavour("<a href=x>y</a>"), Some(Flavour::Html), "unquoted");
        assert_eq!(flavour("<!DOCTYPE html><html></html>"), Some(Flavour::Html));
    }

    #[test]
    fn a_less_than_sign_in_prose_is_not_a_tag() {
        for value in [
            "a < b and b > c",
            "1 < 2",
            "not markup at all",
            "<p>unclosed",
            "</p>",
            "<p>a</div>",
            "<a><b></a></b>",
            "<p>x</p> trailing <",
            "",
        ] {
            assert!(reindent(value).is_none(), "claimed {value:?}");
        }
    }

    /// A document with no element has nothing to lay out, and calling it
    /// markup would put a word in the title that means nothing.
    #[test]
    fn text_alone_is_not_a_document() {
        assert!(reindent("<!-- just a comment -->").is_none());
        assert!(reindent("plain words").is_none());
    }

    #[test]
    fn a_script_holds_text_and_not_markup() {
        assert_eq!(
            shown("<div><script>if (a<b) {}</script></div>").unwrap(),
            ["<div>", "  <script>if (a<b) {}</script>", "</div>"]
        );
    }

    #[test]
    fn comments_and_doctypes_are_kept_whole() {
        assert_eq!(
            shown("<a><!-- note --><b/></a>").unwrap(),
            ["<a>", "  <!-- note -->", "  <b/>", "</a>"]
        );
    }

    #[test]
    fn the_pieces_say_what_each_run_is() {
        let (document, _) = reindent(r#"<a href="x">hi</a>"#).unwrap();
        let kinds: Vec<Kind> = document[0].iter().map(|piece| piece.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Punct, // <
                Kind::Name,  // a
                Kind::Punct, // space
                Kind::Attr,  // href
                Kind::Punct, // =
                Kind::Str,   // "x"
                Kind::Punct, // >
                Kind::Text,  // hi
                Kind::Punct, // </
                Kind::Name,  // a
                Kind::Punct, // >
            ]
        );
    }

    #[test]
    fn nesting_past_the_cap_is_refused_rather_than_recursed_into() {
        let deep = "<a>".repeat(MAX_DEPTH + 2) + &"</a>".repeat(MAX_DEPTH + 2);
        assert!(reindent(&deep).is_none());
    }

    #[test]
    fn whitespace_between_elements_is_the_formatters_to_decide() {
        assert_eq!(
            shown("<a>\n  <b/>\n</a>").unwrap(),
            ["<a>", "  <b/>", "</a>"],
            "an already-indented document comes back indented once"
        );
    }
}
