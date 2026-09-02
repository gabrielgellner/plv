//! Where each row starts, so a page does not have to be counted up to.
//!
//! A CSV has nowhere to seek to: a slice at offset *n* has to parse *n* rows
//! first. On a 30GB file that is 46 seconds for the last page, against under a
//! millisecond for the same data as parquet, where the reader seeks by row
//! group.
//!
//! plv already reads the whole file when it opens one, to fill in the row
//! count. This makes that scan produce a sparse map as well — the byte offset
//! of every [`STRIDE`]-th row — so a page can seek to the nearest checkpoint
//! and parse forward a bounded number of rows instead of all of them. The map
//! costs a few thousand `u64`s for a file of hundreds of millions of rows.
//!
//! The scan is quote-aware, because a newline inside a quoted field is not the
//! end of a record. It counts records the way `data/writer.rs` does and the way
//! Polars does — a blank line is a record — and a test holds all three to the
//! same answer.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};

/// Rows between checkpoints. A page seeks to one and parses forward at most
/// this many, so it trades a bounded read against a few bytes of index: at
/// 272M rows this is roughly four thousand entries.
pub const STRIDE: usize = 65_536;

/// A sparse map from row number to byte offset.
#[derive(Debug)]
pub struct RowIndex {
    /// `checkpoints[k]` is where data row `k * STRIDE` begins.
    checkpoints: Vec<u64>,
    rows: usize,
    /// The header line, kept so a page read out of the middle of the file
    /// still arrives with its column names attached.
    header: Vec<u8>,
    /// Length of the file the scan saw.
    len: u64,
}

impl RowIndex {
    /// Data rows in the file, the header excluded.
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// The last checkpoint at or before `row`, as `(row number, byte offset)`.
    pub fn seek(&self, row: usize) -> (usize, u64) {
        let k = (row / STRIDE).min(self.checkpoints.len().saturating_sub(1));
        match self.checkpoints.get(k) {
            Some(&at) => (k * STRIDE, at),
            None => (0, 0),
        }
    }

    /// Where to stop reading for a page ending just before `row`: the first
    /// checkpoint at or after it, or the end of the file.
    pub fn end_of(&self, row: usize) -> u64 {
        let k = row.div_ceil(STRIDE);
        self.checkpoints.get(k).copied().unwrap_or(self.len)
    }

    /// Scan `path`, counting records and noting where every [`STRIDE`]-th one
    /// begins.
    ///
    /// Streams: it holds the index and a read buffer, nothing else.
    pub fn build(path: &Path, separator: u8) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(1 << 20, file);

        let mut scan = Records::new(separator);
        let mut position: u64 = 0;
        let mut header = Vec::new();
        let mut rows = 0usize;
        let mut checkpoints = Vec::new();
        // The first record is the header; data begins after it.
        let mut past_header = false;
        let mut started = false;

        loop {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            let read = chunk.len();
            let base = position;

            // The header is copied out in its own pass so the hot loop below
            // does not test for it once per byte of a multi-gigabyte file.
            if !past_header {
                for (i, &byte) in chunk.iter().enumerate() {
                    header.push(byte);
                    started = true;
                    if scan.push(byte) {
                        past_header = true;
                        // Trim the terminator: the header goes back in front
                        // of a page, and a newline is added there.
                        while header.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
                            header.pop();
                        }
                        started = false;
                        position = base + i as u64 + 1;
                        break;
                    }
                }
                if !past_header {
                    position = base + read as u64;
                    reader.consume(read);
                    continue;
                }
            }

            let from = (position - base) as usize;
            for (i, &byte) in chunk[from..].iter().enumerate() {
                started = true;
                if scan.push(byte) {
                    rows += 1;
                    started = false;
                    // Row 0 begins where the header ends, added once at the end.
                    if rows % STRIDE == 0 {
                        checkpoints.push(base + (from + i) as u64 + 1);
                    }
                }
            }
            position = base + read as u64;
            reader.consume(read);
        }

        // A file whose last line has no terminator still ends a record.
        if started && scan.in_record() {
            if past_header {
                rows += 1;
            } else {
                past_header = true;
            }
        }

        Ok(Self {
            checkpoints: with_first(checkpoints, &header, len),
            rows,
            header,
            len,
        })
    }
}

/// The offsets recorded above are the starts of rows `STRIDE`, `2*STRIDE`, …
/// because row 0 begins where the header ends. Put that in front.
fn with_first(mut checkpoints: Vec<u64>, header: &[u8], len: u64) -> Vec<u64> {
    checkpoints.insert(0, header_end(header, len));
    checkpoints
}

/// Byte offset of the first data row.
fn header_end(header: &[u8], len: u64) -> u64 {
    // The header was captured without its terminator; a file that is nothing
    // but a header has no data row to point at.
    let after = header.len() as u64 + 1;
    after.min(len)
}

/// Tracks whether a byte ends a record, which needs enough of the grammar to
/// know when a `"` opens a quoted field: only one at the start of a field
/// does, so `ab"cd` is three ordinary characters and not an opening quote.
struct Records {
    separator: u8,
    in_quotes: bool,
    /// A `"` inside a quoted field, waiting to see whether the next byte
    /// doubles it (an escape) or not (the closing quote).
    pending_quote: bool,
    at_field_start: bool,
    in_record: bool,
}

impl Records {
    fn new(separator: u8) -> Self {
        Self {
            separator,
            in_quotes: false,
            pending_quote: false,
            at_field_start: true,
            in_record: false,
        }
    }

    fn in_record(&self) -> bool {
        self.in_record
    }

    /// Feed one byte; true when it closed a record.
    #[inline(always)]
    fn push(&mut self, byte: u8) -> bool {
        if self.pending_quote {
            self.pending_quote = false;
            if byte == b'"' {
                return false; // an escaped quote, still inside the field
            }
            self.in_quotes = false;
            // fall through: this byte is read outside the quotes
        } else if self.in_quotes {
            if byte == b'"' {
                self.pending_quote = true;
            }
            return false;
        }

        if byte == b'\n' {
            self.at_field_start = true;
            self.in_record = false;
            return true;
        }
        self.in_record = true;
        if byte == b'"' && self.at_field_start {
            self.in_quotes = true;
            self.at_field_start = false;
        } else {
            self.at_field_start = byte == self.separator;
        }
        false
    }
}

/// Read the bytes of one page, with the header in front so it parses as a
/// table in its own right.
pub fn read_span(path: &Path, index: &RowIndex, from: u64, to: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut buffer = Vec::with_capacity((to.saturating_sub(from) as usize).saturating_add(64));
    buffer.extend_from_slice(index.header());
    buffer.push(b'\n');
    file.take(to.saturating_sub(from))
        .read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("plv-index-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Named for its contents: these tests run in parallel, and a shared
    /// filename means they overwrite each other's fixture.
    fn rows_of(contents: &str) -> usize {
        let name = format!(
            "count-{:x}.csv",
            contents
                .bytes()
                .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
        );
        RowIndex::build(&write(&name, contents), b',')
            .unwrap()
            .rows()
    }

    #[test]
    fn records_are_counted_the_way_the_writer_counts_them() {
        assert_eq!(rows_of("a,b\n1,2\n3,4\n"), 2);
        assert_eq!(rows_of("a,b\n1,2"), 1, "no trailing newline");
        assert_eq!(rows_of("a,b\n"), 0, "header only");
        assert_eq!(rows_of("a,b\n1,2\n\n3,4\n"), 3, "a blank line is a row");
        assert_eq!(rows_of("a,b\r\n1,2\r\n"), 1, "crlf");
    }

    #[test]
    fn a_newline_inside_quotes_is_not_a_record_boundary() {
        assert_eq!(rows_of("a,b\n\"one\ntwo\",x\ny,z\n"), 2);
        assert_eq!(rows_of("a,b\n\"say \"\"hi\"\"\",x\n"), 1, "escaped quotes");
    }

    #[test]
    fn a_quote_that_is_not_at_a_field_start_is_an_ordinary_character() {
        // `ab"cd` must not open a quoted span, or every later newline would be
        // swallowed and the count would collapse.
        assert_eq!(rows_of("a,b\nab\"cd,x\ny,z\n"), 2);
    }

    /// The index, the writer and Polars all decide separately what a record
    /// is. They have to agree, so this holds the three together over the cases
    /// where they could differ.
    #[test]
    fn the_index_counts_records_the_way_polars_and_the_writer_do() {
        use crate::data::edit::Overlay;
        use crate::data::{loader, writer};

        let cases: &[(&str, &str)] = &[
            ("plain", "a,b\n1,2\n3,4\n"),
            ("no-trailing-newline", "a,b\n1,2\n3,4"),
            ("crlf", "a,b\r\n1,2\r\n3,4\r\n"),
            ("blank-line-between", "a,b\n1,2\n\n3,4\n"),
            ("trailing-blank-line", "a,b\n1,2\n\n"),
            ("quoted-newline", "a,b\n\"one\ntwo\",x\ny,z\n"),
            ("escaped-quotes", "a,b\n\"say \"\"hi\"\"\",x\ny,z\n"),
            ("quoted-separator", "a,b\n\"x,y\",z\np,q\n"),
            ("bare-quote-mid-field", "a,b\nab\"cd,x\ny,z\n"),
            ("empty-fields", "a,b,c\n,,\n1,,3\n"),
        ];

        for (label, contents) in cases {
            let path = write(&format!("agree-{label}.csv"), contents);
            let polars = match loader::load(&path).and_then(|lf| Ok(lf.collect()?)) {
                Ok(df) => df.height(),
                // Polars declines some shapes outright; plv could not open
                // such a file either, so there is nothing to agree about.
                Err(e) => {
                    println!("  {label}: polars will not read this — {e}");
                    continue;
                }
            };
            let index = RowIndex::build(&path, b',').unwrap().rows();
            assert_eq!(
                index, polars,
                "{label}: index says {index}, polars {polars}"
            );

            // The writer refuses when its own count disagrees, so a clean
            // write here is the third opinion.
            writer::splice(
                contents.as_bytes(),
                &mut Vec::new(),
                b',',
                true,
                &Overlay::new(),
                index,
            )
            .unwrap_or_else(|e| panic!("{label}: the writer disagrees — {e}"));
        }
    }

    #[test]
    fn the_header_is_kept_without_its_terminator() {
        let index = RowIndex::build(&write("hdr.csv", "a,b\r\n1,2\n"), b',').unwrap();
        assert_eq!(index.header(), b"a,b");
    }

    #[test]
    fn a_page_can_be_read_back_from_its_offset() {
        let path = write("span.csv", "a,b\n1,2\n3,4\n5,6\n");
        let index = RowIndex::build(&path, b',').unwrap();
        let (row, at) = index.seek(0);
        assert_eq!((row, at), (0, 4), "data starts after `a,b\\n`");

        let bytes = read_span(&path, &index, at, index.end_of(3)).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "a,b\n1,2\n3,4\n5,6\n");
    }

    #[test]
    fn checkpoints_land_on_row_boundaries() {
        // One row per stride is impractical to test at STRIDE; check the
        // arithmetic instead, which is what a page depends on.
        let path = write("stride.csv", "a\n1\n2\n3\n");
        let index = RowIndex::build(&path, b',').unwrap();
        assert_eq!(index.rows(), 3);
        // Everything below one stride resolves to the first checkpoint.
        assert_eq!(index.seek(0), (0, 2));
        assert_eq!(index.seek(2), (0, 2));
        assert_eq!(index.end_of(3), index.len, "past the end is the file's end");
    }
}
