//! Writing pending edits back to a delimited text file.
//!
//! An edit is a *text* edit. The file on disk is untyped; the types plv shows
//! come from Polars' inference over it. So the write path never re-serializes
//! the frame — that would reformat every line, turning a one-cell change into a
//! whole-file diff (`1.10` becomes `1.1`, quoting style changes, nulls and
//! empty strings stop being distinguishable). Instead the original bytes are
//! streamed through and only the edited fields are replaced. Quoting style,
//! line endings, a missing final newline, a BOM, stray whitespace and the
//! header all survive untouched, and the diff covers exactly the cells that
//! were changed.
//!
//! Streaming also keeps the write O(1) in memory, which matters for the same
//! reason the read path is lazy: the file may not fit in it.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use super::edit::Overlay;

/// What a file looked like when plv opened it, so a write cannot clobber
/// changes made underneath it by something else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    len: u64,
    mtime: Option<SystemTime>,
}

impl Stamp {
    pub fn of(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
        Ok(Self {
            len: meta.len(),
            mtime: meta.modified().ok(),
        })
    }

    /// True when `path` still looks the way it did when this stamp was taken.
    pub fn still_matches(&self, path: &Path) -> bool {
        Stamp::of(path).is_ok_and(|now| now == *self)
    }
}

/// Splice `overlay` into `src` and replace `dst` with the result.
///
/// The new file is built beside `dst` and moved into place with a rename, so an
/// interrupted or failed write leaves the original intact. `src` and `dst` are
/// usually the same path; they differ for `:w <path>`.
pub fn save(
    src: &Path,
    dst: &Path,
    separator: u8,
    has_header: bool,
    overlay: &Overlay,
    expected_rows: usize,
) -> Result<Stamp> {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let name = dst
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plv-output");
    let tmp = dir.join(format!(".{name}.plv-tmp"));

    let result = (|| -> Result<()> {
        let input = BufReader::new(
            File::open(src).with_context(|| format!("cannot read {}", src.display()))?,
        );
        let file =
            File::create(&tmp).with_context(|| format!("cannot write beside {}", dst.display()))?;
        let mut output = BufWriter::new(file);
        splice(
            input,
            &mut output,
            separator,
            has_header,
            overlay,
            expected_rows,
        )?;
        output.flush()?;
        // Durable before the rename: a crash should leave either the old file
        // or the new one, never a truncated new one.
        output
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Keep the original's mode; a fresh temp file gets the process umask.
    if let Ok(meta) = fs::metadata(dst) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, dst).with_context(|| format!("cannot replace {}", dst.display()))?;
    Stamp::of(dst)
}

/// Copy `src` to `out`, replacing the fields named by `overlay`.
///
/// Fails without writing anything usable if the record count disagrees with
/// `expected_rows`, or if an edit names a field its record does not have. Both
/// mean plv and this parser read the file differently, and guessing at that
/// point would corrupt data.
pub fn splice<R: BufRead, W: Write>(
    mut src: R,
    out: W,
    separator: u8,
    has_header: bool,
    overlay: &Overlay,
    expected_rows: usize,
) -> Result<()> {
    let mut splicer = Splicer::new(out, separator, has_header, overlay);
    loop {
        let chunk = src.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let read = chunk.len();
        for &byte in chunk {
            splicer.byte(byte)?;
        }
        src.consume(read);
    }
    let (rows, applied, dropped, added) = splicer.finish()?;

    if rows != expected_rows {
        bail!(
            "refusing to write: the file holds {rows} data rows but the view has \
             {expected_rows} — it may have changed on disk"
        );
    }
    if applied != overlay.len() {
        bail!(
            "refusing to write: {} of {} edits had no field to land in \
             (a row with fewer fields than the header)",
            overlay.len() - applied,
            overlay.len()
        );
    }
    if added != overlay.added_count() {
        bail!(
            "refusing to write: {} of {} added rows had nowhere to go",
            overlay.added_count() - added,
            overlay.added_count()
        );
    }
    if dropped != overlay.struck_count() {
        bail!(
            "refusing to write: {} of {} deleted rows were not found",
            overlay.struck_count() - dropped,
            overlay.struck_count()
        );
    }
    Ok(())
}

/// A byte-at-a-time RFC 4180 scanner that passes its input straight through,
/// substituting the fields the overlay names.
///
/// It tracks just enough structure to know which field it is standing in:
/// quoted spans (which may contain separators and newlines), doubled quotes,
/// and `\r\n` versus a bare `\r` inside a field. Replacement text is emitted at
/// the moment the field starts, and the field's original bytes are dropped
/// until it ends.
struct Splicer<'a, W: Write> {
    out: W,
    separator: u8,
    overlay: &'a Overlay,
    /// Records to skip before data row 0: one when the file has a header.
    header_rows: usize,
    /// Records completed so far, the header included.
    record: usize,
    /// The current record has begun, i.e. bytes have arrived since the last
    /// line ending. Only used to tell a final unterminated record from a file
    /// that ends cleanly.
    record_started: bool,
    field: usize,
    field_started: bool,
    /// The current field is being replaced, so its own bytes are dropped.
    replacing: bool,
    in_quotes: bool,
    /// A `"` inside a quoted field whose meaning waits on the next byte:
    /// doubled it is an escape, otherwise it closed the field.
    pending_quote: bool,
    /// A `\r` outside quotes, which is a terminator only if `\n` follows.
    pending_cr: bool,
    /// Edits actually placed, checked against the overlay at the end.
    applied: usize,
    /// The record being read is struck out, so none of it is written — its
    /// terminator included, or the file would grow blank lines where rows
    /// used to be.
    dropping: bool,
    /// Records dropped, checked against the overlay at the end.
    dropped: usize,
    /// Rows added, likewise.
    added: usize,
    /// Fields a new row must have, so it lines up with the header.
    columns: usize,
    /// The line ending the file already uses, so added rows match it.
    newline: &'static [u8],
}

impl<'a, W: Write> Splicer<'a, W> {
    fn new(out: W, separator: u8, has_header: bool, overlay: &'a Overlay) -> Self {
        Self {
            out,
            separator,
            overlay,
            header_rows: usize::from(has_header),
            record: 0,
            record_started: false,
            field: 0,
            field_started: false,
            replacing: false,
            in_quotes: false,
            pending_quote: false,
            pending_cr: false,
            applied: 0,
            dropping: false,
            dropped: 0,
            added: 0,
            columns: 0,
            newline: b"\n",
        }
    }

    fn byte(&mut self, b: u8) -> Result<()> {
        if self.pending_quote {
            self.pending_quote = false;
            if b == b'"' {
                // A doubled quote: an escaped `"` still inside the field.
                self.emit(b"\"\"")?;
                return Ok(());
            }
            // The held quote closed the field; `b` is read outside quotes.
            self.emit(b"\"")?;
            self.in_quotes = false;
        }

        if self.in_quotes {
            if b == b'"' {
                self.pending_quote = true;
            } else {
                self.emit(&[b])?;
            }
            return Ok(());
        }

        if self.pending_cr {
            self.pending_cr = false;
            if b == b'\n' {
                return self.terminator(b"\r\n");
            }
            // Not a line ending after all, so the `\r` was field content.
            self.emit(b"\r")?;
        }

        match b {
            b'\n' => self.terminator(b"\n"),
            b'\r' => {
                self.begin_field()?;
                self.pending_cr = true;
                Ok(())
            }
            _ if b == self.separator => {
                self.begin_field()?;
                self.end_field();
                self.raw(&[b])
            }
            // Only a leading quote opens a quoted field.
            b'"' if !self.field_started => {
                self.begin_field()?;
                self.in_quotes = true;
                self.emit(b"\"")
            }
            _ => {
                self.begin_field()?;
                self.emit(&[b])
            }
        }
    }

    /// Note that a field exists here, and substitute it if the overlay says so.
    ///
    /// Called from the first byte of a field *and* from the separator or
    /// terminator that ends one, so that an empty field — which has no bytes of
    /// its own — is still a place an edit can land.
    fn begin_field(&mut self) -> Result<()> {
        if !self.record_started {
            self.record_started = true;
            self.field = 0;
            self.field_started = false;
            if self.record >= self.header_rows {
                // Rows added before this one go in ahead of it.
                self.write_added(self.record - self.header_rows)?;
            }
            // Decided once per record: a struck row is written nowhere.
            self.dropping = self.record >= self.header_rows
                && self.overlay.is_struck(self.record - self.header_rows);
            if self.dropping {
                self.dropped += 1;
            }
        }
        if self.field_started {
            return Ok(());
        }
        self.field_started = true;
        self.replacing = false;

        if self.record >= self.header_rows
            && let Some(value) = self
                .overlay
                .get((self.record - self.header_rows, self.field))
        {
            self.replacing = true;
            self.applied += 1;
            if !self.dropping {
                let encoded = encode(value, self.separator);
                self.out.write_all(encoded.as_bytes())?;
            }
        }
        Ok(())
    }

    /// Write the rows added before source row `before`, if any.
    ///
    /// A new row is a row of the overlay like any other, so its cells come
    /// from the same place an edit does — and the fields it never had typed
    /// into come out empty, which is what an absent value is in this format.
    fn write_added(&mut self, before: usize) -> Result<()> {
        for &id in self.overlay.added_at(before) {
            let cells = self.overlay.row(id);
            for field in 0..self.columns {
                if field > 0 {
                    self.write_out(&[self.separator])?;
                }
                if let Some(value) = cells.and_then(|cells| cells.get(&field)) {
                    let encoded = encode(value, self.separator);
                    self.write_out(encoded.as_bytes())?;
                    // Counted like any other cell written, so the tally at the
                    // end covers a new row's contents too.
                    self.applied += 1;
                }
            }
            self.write_out(self.newline)?;
            self.added += 1;
        }
        Ok(())
    }

    fn end_field(&mut self) {
        self.field += 1;
        self.field_started = false;
        self.replacing = false;
    }

    /// Every line ending closes a record, blank lines included: Polars reads an
    /// empty line as a row of nulls rather than skipping it, and the row
    /// numbering here has to be the one the viewer is showing.
    fn terminator(&mut self, eol: &'static [u8]) -> Result<()> {
        self.begin_field()?;
        if self.record == 0 {
            // Learned from the header: how many fields a row has, and which
            // line ending this file uses, so an added row matches both.
            self.columns = self.field + 1;
            self.newline = eol;
        }
        self.end_field();
        self.record += 1;
        self.record_started = false;
        if self.dropping {
            // The terminator goes with the row it ended.
            self.dropping = false;
            return Ok(());
        }
        self.raw(eol)
    }

    /// Records seen, edits placed, rows dropped and rows added.
    fn finish(mut self) -> Result<(usize, usize, usize, usize)> {
        if self.pending_quote {
            self.emit(b"\"")?;
            self.in_quotes = false;
        }
        if self.pending_cr {
            self.pending_cr = false;
            self.emit(b"\r")?;
        }
        // A file whose last line has no terminator still ends a record — and
        // anything added after it needs one putting back first.
        let unterminated = self.record_started;
        if unterminated {
            self.begin_field()?;
            self.end_field();
            self.record += 1;
        }

        // Rows added after the last one in the file.
        let rows = self.record.saturating_sub(self.header_rows);
        if !self.overlay.added_at(rows).is_empty() {
            if unterminated {
                self.write_out(self.newline)?;
            }
            self.write_added(rows)?;
        }
        self.out.flush()?;
        Ok((
            self.record.saturating_sub(self.header_rows),
            self.applied,
            self.dropped,
            self.added,
        ))
    }

    /// Field content: dropped while a replacement stands in for it.
    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        if self.replacing {
            return Ok(());
        }
        self.raw(bytes)
    }

    /// Structure — separators and line endings — which is never replaced, but
    /// which still goes nowhere while a struck row is being read past.
    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        if self.dropping {
            return Ok(());
        }
        self.write_out(bytes)
    }

    fn write_out(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        Ok(())
    }
}

/// Quote `value` if writing it bare would change the shape of the record.
fn encode(value: &str, separator: u8) -> Cow<'_, str> {
    let needs_quotes = value
        .bytes()
        .any(|b| b == separator || b == b'"' || b == b'\n' || b == b'\r');
    if !needs_quotes {
        return Cow::Borrowed(value);
    }
    Cow::Owned(format!("\"{}\"", value.replace('"', "\"\"")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splice `edits` into `input` and return the bytes written.
    fn spliced(input: &str, edits: &[((usize, usize), &str)], rows: usize) -> String {
        run(input, edits, rows, b',').expect("splice failed")
    }

    fn run(
        input: &str,
        edits: &[((usize, usize), &str)],
        rows: usize,
        separator: u8,
    ) -> Result<String> {
        let mut overlay = Overlay::new();
        for &(cell, value) in edits {
            overlay.set([(cell, value.to_string())]);
        }
        let mut out = Vec::new();
        splice(input.as_bytes(), &mut out, separator, true, &overlay, rows)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn an_unedited_file_is_copied_byte_for_byte() {
        // Mixed quoting, CRLF, a BOM and no final newline: all of it must survive.
        let input = "\u{feff}name,note\r\n\"a, b\",  x  \r\n\"say \"\"hi\"\"\",y";
        assert_eq!(spliced(input, &[], 2), input);
    }

    #[test]
    fn only_the_edited_field_changes() {
        let input = "name,count\na,1\nb,2\nc,3\n";
        assert_eq!(
            spliced(input, &[((1, 1), "99")], 3),
            "name,count\na,1\nb,99\nc,3\n"
        );
    }

    #[test]
    fn the_header_is_not_a_data_row() {
        let input = "name,count\na,1\n";
        assert_eq!(spliced(input, &[((0, 0), "z")], 1), "name,count\nz,1\n");
    }

    #[test]
    fn a_quoted_field_is_replaced_whole() {
        let input = "name,note\n\"a, b\",\"keep, me\"\n";
        assert_eq!(
            spliced(input, &[((0, 0), "plain")], 1),
            "name,note\nplain,\"keep, me\"\n"
        );
    }

    #[test]
    fn a_new_value_is_quoted_only_when_it_has_to_be() {
        let input = "a,b\n1,2\n";
        assert_eq!(spliced(input, &[((0, 0), "x,y")], 1), "a,b\n\"x,y\",2\n");
        assert_eq!(
            spliced(input, &[((0, 0), "say \"hi\"")], 1),
            "a,b\n\"say \"\"hi\"\"\",2\n"
        );
        assert_eq!(
            spliced(input, &[((0, 0), "two\nlines")], 1),
            "a,b\n\"two\nlines\",2\n"
        );
        assert_eq!(spliced(input, &[((0, 0), "plain")], 1), "a,b\nplain,2\n");
        assert_eq!(spliced(input, &[((0, 0), "")], 1), "a,b\n,2\n");
    }

    #[test]
    fn a_newline_inside_quotes_does_not_end_the_record() {
        let input = "name,note\n\"a\",\"line one\nline two\"\nb,plain\n";
        assert_eq!(
            spliced(input, &[((1, 1), "edited")], 2),
            "name,note\n\"a\",\"line one\nline two\"\nb,edited\n"
        );
    }

    #[test]
    fn crlf_survives_an_edit() {
        let input = "a,b\r\n1,2\r\n3,4\r\n";
        assert_eq!(spliced(input, &[((0, 1), "9")], 2), "a,b\r\n1,9\r\n3,4\r\n");
    }

    #[test]
    fn a_bare_cr_is_field_content() {
        let input = "a,b\n1\r5,2\n";
        assert_eq!(spliced(input, &[((0, 1), "9")], 1), "a,b\n1\r5,9\n");
        assert_eq!(spliced(input, &[((0, 0), "x")], 1), "a,b\nx,2\n");
    }

    #[test]
    fn an_empty_field_is_a_place_an_edit_can_land() {
        let input = "a,b,c\n1,,3\n";
        assert_eq!(spliced(input, &[((0, 1), "2")], 1), "a,b,c\n1,2,3\n");
    }

    #[test]
    fn a_trailing_empty_field_can_be_edited() {
        let input = "a,b,c\n1,2,\n";
        assert_eq!(spliced(input, &[((0, 2), "3")], 1), "a,b,c\n1,2,3\n");
    }

    #[test]
    fn a_missing_final_newline_stays_missing() {
        let input = "a,b\n1,2";
        assert_eq!(spliced(input, &[((0, 1), "9")], 1), "a,b\n1,9");
    }

    #[test]
    fn a_blank_line_takes_a_row_number_of_its_own() {
        // Polars reads an empty line as a row of nulls, so `y,2` is row 2.
        let input = "a,b\nx,1\n\ny,2\n";
        assert_eq!(spliced(input, &[((2, 0), "z")], 3), "a,b\nx,1\n\nz,2\n");
    }

    #[test]
    fn a_trailing_blank_line_is_a_row_too() {
        let input = "a,b\nx,1\n\n";
        assert_eq!(spliced(input, &[((0, 0), "z")], 2), "a,b\nz,1\n\n");
    }

    #[test]
    fn a_tab_separated_file_quotes_on_tabs_not_commas() {
        let input = "a\tb\n1\t2\n";
        assert_eq!(
            run(input, &[((0, 0), "x,y")], 1, b'\t').unwrap(),
            "a\tb\nx,y\t2\n"
        );
        assert_eq!(
            run(input, &[((0, 0), "x\ty")], 1, b'\t').unwrap(),
            "a\tb\n\"x\ty\"\t2\n"
        );
    }

    #[test]
    fn several_edits_in_one_record() {
        let input = "a,b,c\n1,2,3\n";
        assert_eq!(
            spliced(input, &[((0, 0), "x"), ((0, 2), "z")], 1),
            "a,b,c\nx,2,z\n"
        );
    }

    /// Deleting rows, spliced the same way an edit is: the surviving bytes are
    /// untouched and the struck rows leave nothing behind, terminator
    /// included.
    fn without(input: &str, struck: &[usize], rows: usize) -> String {
        let mut overlay = Overlay::new();
        overlay.delete(struck.iter().copied(), usize::MAX);
        let mut out = Vec::new();
        splice(input.as_bytes(), &mut out, b',', true, &overlay, rows).expect("splice failed");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_struck_row_leaves_nothing_behind() {
        let input = "name,count\na,1\nb,2\nc,3\n";
        assert_eq!(without(input, &[1], 3), "name,count\na,1\nc,3\n");
        assert_eq!(without(input, &[0, 2], 3), "name,count\nb,2\n");
        assert_eq!(without(input, &[0, 1, 2], 3), "name,count\n");
    }

    #[test]
    fn the_header_is_never_struck() {
        // Row 0 is the first *data* row, so deleting it must not take the
        // header with it.
        let input = "name,count\na,1\nb,2\n";
        assert_eq!(without(input, &[0], 2), "name,count\nb,2\n");
    }

    #[test]
    fn deleting_a_row_with_a_quoted_newline_removes_all_of_it() {
        let input = "a,b\n\"one\ntwo\",x\ny,z\n";
        assert_eq!(without(input, &[0], 2), "a,b\ny,z\n");
    }

    #[test]
    fn crlf_survives_a_deletion() {
        let input = "a,b\r\n1,2\r\n3,4\r\n";
        assert_eq!(without(input, &[0], 2), "a,b\r\n3,4\r\n");
    }

    #[test]
    fn deleting_the_last_row_of_a_file_with_no_final_newline() {
        let input = "a,b\n1,2\n3,4";
        assert_eq!(without(input, &[1], 2), "a,b\n1,2\n");
        assert_eq!(without(input, &[0], 2), "a,b\n3,4");
    }

    #[test]
    fn a_row_can_be_edited_and_struck_in_the_same_write() {
        let mut overlay = Overlay::new();
        overlay.set([((0, 1), "99".to_string())]);
        overlay.delete([1], usize::MAX);
        let mut out = Vec::new();
        splice(
            "name,count\na,1\nb,2\nc,3\n".as_bytes(),
            &mut out,
            b',',
            true,
            &overlay,
            3,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "name,count\na,99\nc,3\n",
            "the edit lands and the other row goes"
        );
    }

    #[test]
    fn an_edit_on_a_struck_row_is_written_nowhere() {
        // Contradictory, and the row wins: it is not in the file to edit.
        let mut overlay = Overlay::new();
        overlay.set([((1, 0), "zz".to_string())]);
        overlay.delete([1], usize::MAX);
        let mut out = Vec::new();
        splice(
            "name,count\na,1\nb,2\n".as_bytes(),
            &mut out,
            b',',
            true,
            &overlay,
            2,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "name,count\na,1\n");
    }

    #[test]
    fn a_deletion_that_finds_no_row_is_refused() {
        let mut overlay = Overlay::new();
        overlay.delete([9], usize::MAX);
        let mut out = Vec::new();
        let e = splice("a,b\n1,2\n".as_bytes(), &mut out, b',', true, &overlay, 1)
            .unwrap_err()
            .to_string();
        assert!(e.contains("deleted rows were not found"), "{e}");
    }

    /// Adding rows, spliced in at the row they were put before.
    fn with_added(input: &str, at: &[(usize, &[(usize, &str)])], rows: usize) -> String {
        let mut overlay = Overlay::new();
        for &(before, cells) in at {
            let id = overlay.add_row(before, usize::MAX, rows);
            for &(field, value) in cells {
                overlay.set([((id, field), value.to_string())]);
            }
        }
        let mut out = Vec::new();
        splice(input.as_bytes(), &mut out, b',', true, &overlay, rows).expect("splice failed");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_added_row_goes_in_before_the_row_it_was_put_before() {
        let input = "name,count\na,1\nb,2\n";
        assert_eq!(
            with_added(input, &[(1, &[(0, "new"), (1, "9")])], 2),
            "name,count\na,1\nnew,9\nb,2\n"
        );
        assert_eq!(
            with_added(input, &[(0, &[(0, "first")])], 2),
            "name,count\nfirst,\na,1\nb,2\n",
            "before the first row, and still under the header"
        );
    }

    #[test]
    fn a_row_added_past_the_last_one_lands_at_the_end() {
        let input = "name,count\na,1\nb,2\n";
        assert_eq!(
            with_added(input, &[(2, &[(0, "last"), (1, "3")])], 2),
            "name,count\na,1\nb,2\nlast,3\n"
        );
    }

    #[test]
    fn an_added_row_has_a_field_for_every_column() {
        // Untyped fields come out empty, which is what an absent value is.
        let input = "a,b,c\n1,2,3\n";
        assert_eq!(
            with_added(input, &[(1, &[(1, "only")])], 1),
            "a,b,c\n1,2,3\n,only,\n"
        );
    }

    #[test]
    fn added_rows_keep_the_files_own_line_ending() {
        let input = "a,b\r\n1,2\r\n";
        assert_eq!(
            with_added(input, &[(1, &[(0, "x"), (1, "y")])], 1),
            "a,b\r\n1,2\r\nx,y\r\n"
        );
    }

    #[test]
    fn a_row_added_to_a_file_with_no_final_newline_gets_one_first() {
        let input = "a,b\n1,2";
        assert_eq!(
            with_added(input, &[(1, &[(0, "x"), (1, "y")])], 1),
            "a,b\n1,2\nx,y\n",
            "or the added row would join the last one"
        );
    }

    #[test]
    fn several_rows_added_at_one_place_keep_their_order() {
        let input = "a,b\n1,2\n";
        let mut overlay = Overlay::new();
        for name in ["one", "two", "three"] {
            let id = overlay.add_row(1, usize::MAX, 1);
            overlay.set([((id, 0), name.to_string())]);
        }
        let mut out = Vec::new();
        splice(input.as_bytes(), &mut out, b',', true, &overlay, 1).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "a,b\n1,2\none,\ntwo,\nthree,\n"
        );
    }

    #[test]
    fn a_value_needing_quotes_is_quoted_in_an_added_row() {
        let input = "a,b\n1,2\n";
        assert_eq!(
            with_added(input, &[(1, &[(0, "x,y"), (1, "say \"hi\"")])], 1),
            "a,b\n1,2\n\"x,y\",\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn a_row_added_where_no_row_exists_is_refused() {
        let mut overlay = Overlay::new();
        overlay.add_row(9, 0, 2);
        let mut out = Vec::new();
        let e = splice(
            "a,b\n1,2\n3,4\n".as_bytes(),
            &mut out,
            b',',
            true,
            &overlay,
            2,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("added rows had nowhere to go"), "{e}");
    }

    #[test]
    fn a_row_count_that_disagrees_is_refused() {
        let err = run("a,b\n1,2\n", &[], 5, b',').unwrap_err().to_string();
        assert!(err.contains("1 data rows"), "{err}");
        assert!(err.contains("changed on disk"), "{err}");
    }

    #[test]
    fn an_edit_with_no_field_to_land_in_is_refused() {
        // A ragged row: field 4 does not exist, so the edit would be dropped.
        let err = run("a,b\n1,2\n", &[((0, 4), "x")], 1, b',')
            .unwrap_err()
            .to_string();
        assert!(err.contains("no field to land in"), "{err}");
    }

    #[test]
    fn save_replaces_the_file_atomically_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join("plv-writer-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("save.csv");
        fs::write(&path, "a,b\n1,2\n").unwrap();

        let mut overlay = Overlay::new();
        overlay.set([((0, 1), "9".to_string())]);
        let stamp = save(&path, &path, b',', true, &overlay, 1).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "a,b\n1,9\n");
        assert!(stamp.still_matches(&path));
        assert!(!dir.join(".save.csv.plv-tmp").exists());
    }

    #[test]
    fn a_refused_save_leaves_the_original_untouched() {
        let dir = std::env::temp_dir().join("plv-writer-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("refused.csv");
        fs::write(&path, "a,b\n1,2\n").unwrap();

        let err = save(&path, &path, b',', true, &Overlay::new(), 7).unwrap_err();
        assert!(err.to_string().contains("changed on disk"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "a,b\n1,2\n");
        assert!(!dir.join(".refused.csv.plv-tmp").exists());
    }

    /// The row numbering here has to match the one Polars hands the viewer,
    /// or an edit lands on the wrong line. Checked against the real reader
    /// rather than assumed, because blank lines are where the two could part
    /// company.
    #[test]
    fn record_numbering_agrees_with_the_polars_reader() {
        use crate::data::loader;

        let dir = std::env::temp_dir().join("plv-writer-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.csv");
        // Blank lines, a quoted embedded newline and a quoted separator: every
        // way the two parsers could disagree about what a record is.
        let source = "name,note\nx,1\n\n\"a, b\",\"two\nlines\"\nz,3\n";
        fs::write(&path, source).unwrap();

        let df = loader::load(&path).unwrap().collect().unwrap();
        let rows = df.height();

        // Edit the last row, whichever index Polars gave it.
        let mut overlay = Overlay::new();
        overlay.set([((rows - 1, 1), "edited".to_string())]);
        let separator = loader::separator(&path).unwrap().unwrap();
        save(&path, &path, separator, true, &overlay, rows).unwrap();

        let after = loader::load(&path).unwrap().collect().unwrap();
        assert_eq!(after.height(), rows, "the write changed the row count");
        let note = after.column("note").unwrap().str().unwrap();
        assert_eq!(note.get(rows - 1), Some("edited"));
        // The untouched rows came through unchanged.
        let name = after.column("name").unwrap().str().unwrap();
        assert_eq!(name.get(0), Some("x"));
        assert_eq!(name.get(rows - 1), Some("z"));
    }

    #[test]
    fn a_stamp_notices_a_file_changing_underneath_it() {
        let dir = std::env::temp_dir().join("plv-writer-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stamp.csv");
        fs::write(&path, "a,b\n1,2\n").unwrap();

        let stamp = Stamp::of(&path).unwrap();
        assert!(stamp.still_matches(&path));

        fs::write(&path, "a,b\n1,2\n3,4\n").unwrap();
        assert!(!stamp.still_matches(&path));
    }
}
