//! The system clipboard, reached through the terminal rather than through a
//! library.
//!
//! A block of cells goes out as **TSV**, which is what a spreadsheet puts on
//! the clipboard when you copy a range and what it expects to receive, and it
//! is quoted by [`writer::encode`] — the same function the writer uses on a
//! field, because the two have to agree about when a value needs quotes.
//!
//! **Out** through OSC 52, an escape sequence that asks the terminal to set
//! its clipboard. **In** through bracketed paste: the terminal sends what was
//! pasted as an ordinary event, which is the read half that OSC 52 does not
//! reliably have. Between them the two directions are covered with no
//! dependency at all — no windowing library to link, nothing to fail on a
//! headless box, and both halves work over SSH, where a native clipboard
//! would be reaching for the wrong machine's.
//!
//! The cost is that the *keys* differ: `y` copies, but pasting in is the
//! terminal's own paste — plv never asks for the clipboard, it is handed to
//! it. Reading OSC 52 back is disabled in most terminals for the obvious
//! reason, so asking was never really on offer.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};

use crate::data::writer;

/// Tab, since a block goes out as TSV.
const SEPARATOR: u8 = b'\t';

/// The most text one `y` will put on the clipboard.
///
/// OSC 52 goes through the terminal's input parser, and terminals bound what
/// they will accept — xterm's limit is around 100kB of base64 and others are
/// lower. Past this the sequence would be dropped or, worse, truncated
/// halfway, so plv does not send it and says so: half a block on the
/// clipboard is worse than none, because nothing about it looks wrong.
pub const MAX_BYTES: usize = 64 * 1024;

/// A block of cells as TSV.
pub fn tsv(block: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in block {
        let line: Vec<Cow<'_, str>> = row
            .iter()
            .map(|value| writer::encode(value, SEPARATOR))
            .collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

/// Ask the terminal to put `text` on the system clipboard.
///
/// `Ok(false)` when the text is too long to send, or when there is no
/// terminal to ask — output redirected somewhere, or a test run. Neither is
/// an error: the internal register still has the block, and the caller says
/// what happened.
pub fn copy(text: &str) -> io::Result<bool> {
    if text.len() > MAX_BYTES || !io::stdout().is_terminal() {
        return Ok(false);
    }
    let mut out = io::stdout().lock();
    write!(out, "{}", sequence(text))?;
    out.flush()?;
    Ok(true)
}

/// The OSC 52 sequence that carries `text`.
///
/// `c` is the clipboard selection; the terminator is BEL, which every
/// terminal that implements this at all accepts, where the more correct ST is
/// patchier.
fn sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// A pasted block, as rows of cell text.
///
/// The inverse of [`tsv`], and it has to be: a value plv quoted on the way
/// out must come back the value it was. Rows are lines, cells are tabs, and a
/// field that opens with a quote runs to its closing one — so a cell holding
/// a newline survives the round trip instead of arriving as two rows.
///
/// Anything else pasted is one cell of text, which is what a paste of one
/// value from anywhere else is.
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if !started => quoted = true,
            '\t' => {
                row.push(std::mem::take(&mut field));
                started = false;
                continue;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                started = false;
                continue;
            }
            _ => field.push(c),
        }
        started = true;
    }
    // A last line with no terminator is still a row; a trailing newline has
    // already closed its own.
    if started || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Standard base64, which is what OSC 52 carries.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            // A chunk of one byte fills two characters and pads two; a chunk
            // of two fills three and pads one.
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - i * 6)) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|c| c.to_string()).collect())
            .collect()
    }

    #[test]
    fn a_block_goes_out_as_tsv() {
        assert_eq!(tsv(&block(&[&["a", "b"], &["c", "d"]])), "a\tb\nc\td\n");
    }

    /// The wire format and the file writer have to agree about quoting, or a
    /// cell holding a tab arrives somewhere else as two.
    #[test]
    fn a_value_that_needs_quotes_gets_them_and_survives_the_trip() {
        let awkward = block(&[&["one\ttwo", "a \"quoted\" thing"], &["line\nbreak", ""]]);
        let wire = tsv(&awkward);
        assert_eq!(
            wire,
            "\"one\ttwo\"\t\"a \"\"quoted\"\" thing\"\n\"line\nbreak\"\t\n"
        );
        assert_eq!(parse(&wire), awkward, "and comes back what it was");
    }

    #[test]
    fn a_plain_block_round_trips() {
        let plain = block(&[&["1", "2", "3"], &["4", "5", "6"]]);
        assert_eq!(parse(&tsv(&plain)), plain);
    }

    #[test]
    fn a_paste_from_anywhere_else_is_read_as_it_comes() {
        assert_eq!(parse("hello"), block(&[&["hello"]]), "one value, one cell");
        assert_eq!(parse("a\tb"), block(&[&["a", "b"]]));
        assert_eq!(
            parse("a\tb\r\nc\td\r\n"),
            block(&[&["a", "b"], &["c", "d"]]),
            "and a windows line ending is still a line ending"
        );
        assert_eq!(
            parse("a\nb"),
            block(&[&["a"], &["b"]]),
            "a last line with no terminator is still a row"
        );
        assert!(parse("").is_empty(), "and nothing is nothing");
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }

    #[test]
    fn the_sequence_is_the_one_terminals_answer() {
        assert_eq!(sequence("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn too_much_text_is_not_sent_at_all() {
        // Half a block on the clipboard is worse than none: nothing about it
        // looks wrong.
        assert!(!copy(&"x".repeat(MAX_BYTES + 1)).unwrap());
    }
}
