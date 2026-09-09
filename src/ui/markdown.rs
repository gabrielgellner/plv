//! Reading markdown in a cell, without rewriting it.
//!
//! The other two formats lay a document out: the structure *is* the content,
//! and on one line it has nowhere to show. Markdown is not like that — it was
//! designed to be read as it is written, so there is no indentation to add and
//! nothing to move. What plv can do is say which parts are which: a heading in
//! bold, a fence in the colour code is drawn in, a bullet's dash out of the
//! way of the words after it.
//!
//! **Every marker stays where it was.** `##` and `**` are drawn dim rather
//! than hidden, so the text on screen is the text in the cell, character for
//! character — a test holds it to that. Hiding them would be the first place
//! plv showed something other than what a cell holds, and it would take the
//! difference between a `*` in prose and a `*` that made a bullet with it. The
//! promise is the same one the other formats make, kept a different way: they
//! add only line breaks, and this adds only colour.
//!
//! **Detection is a judgement, not a parse**, and it is the only one in here.
//! Almost any prose is valid markdown — a paragraph is a markdown document —
//! so parsing cannot be the test. What is asked instead is whether there is
//! *structure worth drawing*: a heading, a fence, a quote, or more than one
//! list item. Getting that wrong is cheap in a way the others are not, since
//! nothing is hidden either way: a false positive costs a dim dash.

use super::syntax::{Kind, Piece};

/// The value with its parts named, or `None` when there is no structure in it
/// worth drawing.
pub fn format(value: &str) -> Option<Vec<Vec<Piece>>> {
    let source: Vec<&str> = value.split('\n').collect();
    if !worth_drawing(&source) {
        return None;
    }

    let mut out = Vec::with_capacity(source.len());
    let mut fenced = false;
    for line in source {
        if let Some(fence) = fence_at(line) {
            fenced = !fenced;
            // The fence and its info string: structure, and the name of a
            // language plv does not pretend to read.
            let mut pieces = vec![Piece::new(Kind::Punct, fence)];
            if fence.len() < line.len() {
                pieces.push(Piece::new(Kind::Code, &line[fence.len()..]));
            }
            out.push(pieces);
            continue;
        }
        if fenced {
            out.push(vec![Piece::new(Kind::Code, line)]);
            continue;
        }
        out.push(block(line));
    }
    Some(out)
}

/// Whether the value has markdown in it, rather than merely being valid as
/// markdown — which prose is.
///
/// A lone dash at the start of a line is a sentence as often as it is a
/// bullet, so one list item is not enough; two is a list.
fn worth_drawing(lines: &[&str]) -> bool {
    let mut bullets = 0;
    for line in lines {
        if heading_at(line).is_some() || fence_at(line).is_some() || quote_at(line).is_some() {
            return true;
        }
        if bullet_at(line).is_some() {
            bullets += 1;
            if bullets > 1 {
                return true;
            }
        }
    }
    false
}

/// One line: its own marker, if it has one, and then its text.
fn block(line: &str) -> Vec<Piece> {
    if let Some(marker) = heading_at(line) {
        let mut pieces = vec![Piece::new(Kind::Punct, marker)];
        pieces.push(Piece::new(Kind::Heading, &line[marker.len()..]));
        return pieces;
    }
    // A quote or a bullet: its marker, then the words after it read as any
    // other line is.
    if let Some(marker) = quote_at(line).or_else(|| bullet_at(line)) {
        let mut pieces = vec![Piece::new(Kind::Punct, marker)];
        pieces.extend(inline(&line[marker.len()..]));
        return pieces;
    }
    inline(line)
}

/// `#` through `######` and the space after them, or `None`.
fn heading_at(line: &str) -> Option<&str> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=6).contains(&hashes).then_some(())?;
    let rest = &line[hashes..];
    let spaces = rest.len() - rest.trim_start_matches(' ').len();
    (spaces > 0).then(|| &line[..hashes + spaces])
}

/// The ``` or ~~~ that opens or closes a fence.
fn fence_at(line: &str) -> Option<&str> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    for mark in ["```", "~~~"] {
        if rest.starts_with(mark) {
            let run = rest.len() - rest.trim_start_matches(&mark[..1]).len();
            return Some(&line[..indent + run]);
        }
    }
    None
}

fn quote_at(line: &str) -> Option<&str> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    if !rest.starts_with('>') {
        return None;
    }
    let after = &rest[1..];
    let spaces = after.len() - after.trim_start_matches(' ').len();
    Some(&line[..indent + 1 + spaces.min(1)])
}

/// `- `, `* `, `+ ` or `1. `, with whatever indentation it sits at.
fn bullet_at(line: &str) -> Option<&str> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let mark = match rest.as_bytes().first()? {
        b'-' | b'*' | b'+' => 1,
        b'0'..=b'9' => {
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            match rest.as_bytes().get(digits) {
                Some(b'.' | b')') => digits + 1,
                _ => return None,
            }
        }
        _ => return None,
    };
    let after = &rest[mark..];
    let spaces = after.len() - after.trim_start_matches(' ').len();
    (spaces > 0).then(|| &line[..indent + mark + spaces])
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// A line's text, with the spans inside it named.
///
/// Every character comes back exactly once: a marker as [`Kind::Punct`] and
/// what it marks as itself, so the line still reads as it was written.
fn inline(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut at = 0usize;

    while at < text.len() {
        let (mark, kind) = match bytes[at] {
            b'`' => ("`", Kind::Code),
            b'*' if bytes.get(at + 1) == Some(&b'*') => ("**", Kind::Strong),
            b'_' if bytes.get(at + 1) == Some(&b'_') => ("__", Kind::Strong),
            b'*' => ("*", Kind::Emphasis),
            b'_' => ("_", Kind::Emphasis),
            _ => {
                let next = text[at..].chars().next().map_or(1, char::len_utf8);
                plain.push_str(&text[at..at + next]);
                at += next;
                continue;
            }
        };
        // A marker with no partner on this line is an asterisk, not emphasis.
        let from = at + mark.len();
        let Some(end) = text[from..].find(mark).map(|found| from + found) else {
            plain.push_str(mark);
            at += mark.len();
            continue;
        };
        if end == from {
            // `**` with nothing between: two markers and no span.
            plain.push_str(&text[at..end + mark.len()]);
            at = end + mark.len();
            continue;
        }
        if !plain.is_empty() {
            pieces.push(Piece::new(Kind::Text, std::mem::take(&mut plain)));
        }
        pieces.push(Piece::new(Kind::Punct, mark));
        pieces.push(Piece::new(kind, &text[from..end]));
        pieces.push(Piece::new(Kind::Punct, mark));
        at = end + mark.len();
    }
    if !plain.is_empty() {
        pieces.push(Piece::new(Kind::Text, plain));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &[Piece]) -> Vec<(Kind, &str)> {
        line.iter()
            .map(|piece| (piece.kind, piece.text.as_str()))
            .collect()
    }

    /// The promise: what is drawn is what the cell holds, character for
    /// character. Only the colour of it changes.
    fn keeps_every_character(value: &str) {
        let document = format(value).expect("recognised");
        let back: Vec<String> = document
            .iter()
            .map(|line| line.iter().map(|piece| piece.text.as_str()).collect())
            .collect();
        assert_eq!(back.join("\n"), value, "the text on screen is the cell's");
    }

    #[test]
    fn every_character_survives_being_named() {
        for value in [
            "# Title\n\nSome *words* and `code`.\n\n- one\n- two\n",
            "## A\ntext **bold** text\n> quoted\n\n```rust\nlet x = 1;\n```\n",
            "- a\n- b",
            "#### four\n1. first\n2. second\n",
            "> quote **with** markers\n> and `code`\n- x\n- y",
        ] {
            keeps_every_character(value);
        }
    }

    #[test]
    fn a_heading_keeps_its_hashes_and_bolds_its_words() {
        let document = format("## Deploy\n- a\n- b").unwrap();
        assert_eq!(
            kinds(&document[0]),
            [(Kind::Punct, "## "), (Kind::Heading, "Deploy")]
        );
    }

    #[test]
    fn a_bullet_gets_out_of_the_way_of_its_words() {
        let document = format("- rollback with `plv down`\n- **check** the queue").unwrap();
        assert_eq!(
            kinds(&document[0]),
            [
                (Kind::Punct, "- "),
                (Kind::Text, "rollback with "),
                (Kind::Punct, "`"),
                (Kind::Code, "plv down"),
                (Kind::Punct, "`"),
            ]
        );
        assert_eq!(
            kinds(&document[1]),
            [
                (Kind::Punct, "- "),
                (Kind::Punct, "**"),
                (Kind::Strong, "check"),
                (Kind::Punct, "**"),
                (Kind::Text, " the queue"),
            ]
        );
    }

    #[test]
    fn a_fence_holds_code_and_not_markdown() {
        let document = format("```sql\nselect * from t where a = 1\n```").unwrap();
        assert_eq!(
            kinds(&document[0]),
            [(Kind::Punct, "```"), (Kind::Code, "sql")]
        );
        assert_eq!(
            kinds(&document[1]),
            [(Kind::Code, "select * from t where a = 1")],
            "the asterisk inside is not emphasis"
        );
    }

    #[test]
    fn numbered_and_quoted_lines_are_marked_too() {
        let document = format("# T\n1. first\n> said").unwrap();
        assert_eq!(document[1][0], Piece::new(Kind::Punct, "1. "));
        assert_eq!(document[2][0], Piece::new(Kind::Punct, "> "));
    }

    /// Almost any prose is valid markdown, so parsing cannot be the test.
    #[test]
    fn prose_is_not_claimed_as_markdown() {
        for value in [
            "just a note",
            "a sentence with * an asterisk",
            "- a single dash line, which is a sentence as often as a bullet",
            "2 * 3 = 6 and 4 * 5 = 20",
            "an#anchor and a_variable_name",
            "",
        ] {
            assert!(format(value).is_none(), "claimed {value:?}");
        }
    }

    #[test]
    fn a_marker_with_no_partner_is_just_a_character() {
        let document = format("# T\nan * alone and a ` alone").unwrap();
        assert_eq!(
            kinds(&document[1]),
            [(Kind::Text, "an * alone and a ` alone")]
        );
    }

    #[test]
    fn an_empty_span_is_left_as_the_characters_it_is() {
        let document = format("# T\nnothing ** between").unwrap();
        assert_eq!(kinds(&document[1]), [(Kind::Text, "nothing ** between")]);
    }
}
