//! Re-indenting JSON, without rebuilding it.
//!
//! A cell holding `{"id":4821,"tags":["a","b"]}` is a document whose structure
//! *is* its content, and shown as one wrapped paragraph that structure is
//! thrown away. This puts it back: newlines and indentation added, and the
//! token kinds handed out so the window can colour them.
//!
//! **The original bytes are re-emitted, never re-serialised.** Parsing into a
//! model and printing the model back is the usual way to do this and it is
//! lossy in ways that matter to a viewer: key order goes (or costs an ordered
//! map), `1.0` and `1e3` come back as whatever the float formatter prefers,
//! duplicate keys are silently dropped, and escapes are rewritten. plv's
//! writer splices bytes rather than re-serialising a frame for exactly these
//! reasons; a reader that quietly disagreed with it about what is in the file
//! would be worse than one that shows nothing. So every piece of text below
//! is a slice of the value, and the only things added are line breaks and
//! spaces.
//!
//! It is also the parse that detection needs. There is no sniffing here that
//! could be half-right: a value is JSON if this walks all the way to the end
//! of it, and text otherwise.

/// Pieces are the shared ones: a JSON key and an XML element name are the
/// same kind of thing, and one place decides what colour that is.
use super::syntax::{Kind, Piece};

/// Nesting past this is refused rather than recursed into. A cell nested 64
/// deep is not a thing anyone is reading in a table viewer, and the parser
/// has the terminal's stack rather than one of its own.
const MAX_DEPTH: usize = 64;
/// Spaces per level.
const INDENT: usize = 2;

/// The value re-indented, as lines of pieces — or `None` if it is not a JSON
/// object or array.
///
/// A bare `42` or `"hello"` is a valid JSON document and is deliberately not
/// one here: re-indenting a scalar does nothing, and calling every number in
/// the file JSON would put a format in the title that means nothing to the
/// reader.
pub fn reindent(value: &str) -> Option<Vec<Vec<Piece>>> {
    let start = value.find(|c: char| !c.is_whitespace())?;
    if !matches!(value.as_bytes()[start], b'{' | b'[') {
        return None;
    }
    let mut parser = Parser {
        source: value,
        at: start,
        lines: Vec::new(),
        line: Vec::new(),
        depth: 0,
    };
    parser.value(0)?;
    parser.space();
    // Trailing anything means this was not one document, whatever the first
    // half looked like.
    if parser.at != value.len() {
        return None;
    }
    parser.lines.push(parser.line);
    Some(parser.lines)
}

struct Parser<'a> {
    source: &'a str,
    at: usize,
    lines: Vec<Vec<Piece>>,
    line: Vec<Piece>,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.at).copied()
    }

    /// Whitespace between tokens is dropped: what goes back is this module's
    /// own layout, which is the point of the exercise.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> Option<()> {
        (self.peek()? == byte).then(|| self.at += 1)
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        self.line.push(Piece::new(kind, text));
    }

    /// End the line and open the next one at `indent`.
    fn newline(&mut self, indent: usize) {
        self.lines.push(std::mem::take(&mut self.line));
        if indent > 0 {
            self.push(Kind::Punct, " ".repeat(indent * INDENT));
        }
    }

    fn value(&mut self, indent: usize) -> Option<()> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'{' => self.container(indent, true),
            b'[' => self.container(indent, false),
            b'"' => {
                let text = self.string()?;
                self.push(Kind::Str, text);
                Some(())
            }
            b't' => self.literal("true"),
            b'f' => self.literal("false"),
            b'n' => self.literal("null"),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    /// An object or an array — the same walk either way, differing only in
    /// whether each element is preceded by a key.
    fn container(&mut self, indent: usize, object: bool) -> Option<()> {
        let (open, close) = if object { (b'{', b'}') } else { (b'[', b']') };
        self.take(open)?;
        self.space();
        // An empty one stays on its line: two braces and a line break between
        // them says less than two braces.
        if self.peek()? == close {
            self.at += 1;
            self.push(Kind::Punct, if object { "{}" } else { "[]" });
            return Some(());
        }
        self.push(Kind::Punct, if object { "{" } else { "[" });
        self.depth += 1;
        loop {
            self.newline(indent + 1);
            self.space();
            if object {
                let key = self.string()?;
                self.push(Kind::Name, key);
                self.space();
                self.take(b':')?;
                self.push(Kind::Punct, ": ");
                self.space();
            }
            self.value(indent + 1)?;
            self.space();
            match self.peek()? {
                b',' => {
                    self.at += 1;
                    self.push(Kind::Punct, ",");
                }
                byte if byte == close => {
                    self.at += 1;
                    self.newline(indent);
                    self.push(Kind::Punct, if object { "}" } else { "]" });
                    self.depth -= 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// The quoted span, exactly as written — escapes and all. A `\n` inside a
    /// string is two characters of the document and stays two: this is the
    /// document being shown, not the string it decodes to.
    fn string(&mut self) -> Option<String> {
        let start = self.at;
        self.take(b'"')?;
        loop {
            match self.peek()? {
                b'\\' => {
                    // Skip the escape and whatever it escapes, so a `\"` does
                    // not end the string. `\u` needs no special case: its four
                    // digits are ordinary characters after this.
                    self.at += 1;
                    self.peek()?;
                    self.at += 1;
                }
                b'"' => {
                    self.at += 1;
                    return Some(self.source[start..self.at].to_string());
                }
                // A raw control character is not allowed in a JSON string, and
                // letting one through would make a cell holding a stray brace
                // and a newline read as a document.
                byte if byte < 0x20 => return None,
                _ => {
                    // Byte at a time is fine: a multi-byte character has no
                    // ASCII byte in it, so this never splits one.
                    self.at += 1;
                }
            }
        }
    }

    fn literal(&mut self, word: &'static str) -> Option<()> {
        if self.source[self.at..].starts_with(word) {
            self.at += word.len();
            self.push(Kind::Lit, word);
            return Some(());
        }
        None
    }

    /// Numbers are checked in the shape JSON allows and then handed back
    /// verbatim, so `1.0`, `1e3` and `-0` survive as what the file says.
    fn number(&mut self) -> Option<()> {
        let start = self.at;
        self.take(b'-');
        let whole = self.at;
        let digits = self.digits();
        if digits == 0 {
            return None;
        }
        // JSON forbids a leading zero, and letting `01` through would claim a
        // padded id or a zip code as a number in a document.
        if digits > 1 && self.source.as_bytes()[whole] == b'0' {
            return None;
        }
        if self.take(b'.').is_some() && self.digits() == 0 {
            return None;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if self.peek() == Some(b'+') || self.peek() == Some(b'-') {
                self.at += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        let text = self.source[start..self.at].to_string();
        self.push(Kind::Num, text);
        Some(())
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document as it would be drawn, one string per line.
    fn shown(value: &str) -> Option<Vec<String>> {
        Some(
            reindent(value)?
                .iter()
                .map(|line| line.iter().map(|piece| piece.text.as_str()).collect())
                .collect(),
        )
    }

    #[test]
    fn an_object_is_broken_out_one_key_to_a_line() {
        assert_eq!(
            shown(r#"{"id":1,"name":"ada"}"#).unwrap(),
            ["{", "  \"id\": 1,", "  \"name\": \"ada\"", "}"]
        );
    }

    #[test]
    fn nesting_indents() {
        assert_eq!(
            shown(r#"{"a":{"b":[1,2]}}"#).unwrap(),
            [
                "{",
                "  \"a\": {",
                "    \"b\": [",
                "      1,",
                "      2",
                "    ]",
                "  }",
                "}"
            ]
        );
    }

    #[test]
    fn an_empty_container_stays_on_its_line() {
        assert_eq!(
            shown(r#"{"a":{},"b":[]}"#).unwrap(),
            ["{", "  \"a\": {},", "  \"b\": []", "}"]
        );
    }

    /// The whole reason this re-emits slices instead of printing a parsed
    /// model: what the file says is what is shown.
    #[test]
    fn the_documents_own_bytes_come_back_unchanged() {
        let lines = shown(r#"{"b":1.0,"a":2,"b":3,"n":1e3,"z":-0}"#).unwrap();
        assert_eq!(
            lines,
            [
                "{",
                "  \"b\": 1.0,",
                "  \"a\": 2,",
                "  \"b\": 3,",
                "  \"n\": 1e3,",
                "  \"z\": -0",
                "}"
            ],
            "key order kept, duplicates kept, numbers not reformatted"
        );
    }

    #[test]
    fn escapes_and_unicode_survive_as_written() {
        let lines = shown(r#"{"k":"a\"b\\ é é\nx"}"#).unwrap();
        assert_eq!(lines[1], r#"  "k": "a\"b\\ é é\nx""#);
    }

    #[test]
    fn the_pieces_say_what_each_run_is() {
        let doc = reindent(r#"{"k":"v","n":1,"t":true}"#).unwrap();
        let kinds: Vec<Kind> = doc[1].iter().map(|piece| piece.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Punct, Kind::Name, Kind::Punct, Kind::Str, Kind::Punct],
            "indent, key, colon, value, comma"
        );
        assert_eq!(doc[2][3].kind, Kind::Num);
        assert_eq!(doc[3][3].kind, Kind::Lit);
    }

    #[test]
    fn a_value_that_is_not_json_is_not_claimed() {
        // Detection is this parse and nothing else, so everything that fails
        // it falls back to text rather than being shown as a document.
        for value in [
            "hello",
            "42",
            "\"a string\"",
            "{",
            "{}}",
            "{\"a\":1,}",
            "{\"a\" 1}",
            "{a:1}",
            "{\"a\":01}",
            "{\"a\":1} trailing",
            "{\"a\":tru}",
            "{\"a\":1.}",
            "{\"a\":\"unterminated}",
            "not json {\"a\":1}",
        ] {
            assert!(reindent(value).is_none(), "claimed {value:?}");
        }
    }

    #[test]
    fn a_newline_inside_a_string_is_not_a_document() {
        // A raw control character is invalid JSON, and letting it pass would
        // make a note that happens to start with a brace read as one.
        assert!(reindent("{\"a\": \"one\ntwo\"}").is_none());
    }

    #[test]
    fn whitespace_around_the_document_is_allowed_and_replaced() {
        assert_eq!(
            shown("  {\n\t\"a\" : 1 }  ").unwrap(),
            ["{", "  \"a\": 1", "}"]
        );
    }

    #[test]
    fn nesting_past_the_cap_is_refused_rather_than_recursed_into() {
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(reindent(&deep).is_none());
        let shallow = "[".repeat(8) + "1" + &"]".repeat(8);
        assert!(reindent(&shallow).is_some());
    }
}
