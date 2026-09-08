//! Reading JSONL: one JSON object per line, shown as a table.
//!
//! A log is a table that was never written down as one. The grid, `:filter`,
//! `/` and `K` are most of what reading one wants, and the format fits plv's
//! machinery better than CSV does: a JSON string cannot hold a raw newline —
//! control characters have to be escaped, and [`crate::ui::json`] refuses one
//! that is not — so **every `\n` ends a record, exactly**. The index that made
//! 30GB CSVs pageable needs no quote state here at all.
//!
//! **plv parses the records rather than Polars.** Polars' ndjson reader can be
//! given a `String` column and will put a nested value in it, but it writes
//! its own `ValueDisplay` form — `{x: 1}`, unquoted keys, documented as "not
//! guaranteed to be valid JSON". That is the one thing this feature cannot
//! afford: the whole reason to read logs here is that `K` opens the nested
//! field *as a document*, and a cell holding something JSON-shaped but not
//! JSON would not open. So a nested value is carried through as the **exact
//! bytes the file holds**, the same promise `data/writer.rs` makes on the way
//! out.
//!
//! **The columns are the keys, found exactly rather than sampled.** DuckDB
//! reads 20,480 records and infers; VisiData adds a column the moment a new
//! key appears, which moves the table sideways while you are reading it. plv
//! is already reading every byte of the file to build the row index, and that
//! pass finishes before the first frame is drawn — so the key set can be the
//! true one *and* settled before anything is on screen.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::ops::Range;
use std::path::Path;

use anyhow::Result;
use polars::prelude::*;

/// Past this many distinct keys, later ones stop becoming columns.
///
/// ClickHouse's `max_dynamic_paths` default, and for its reason: a log with
/// more paths than this is not a table with that many columns, it is a
/// dictionary, and treating it as a schema helps nobody. What is left out is
/// named rather than dropped in silence.
pub const MAX_COLUMNS: usize = 1024;

/// A key in fewer than this share of the records is available but not shown.
///
/// Splunk's rule for its Interesting Fields sidebar. A log's tail of rare keys
/// is real data and must be reachable — `C` and `:select` reach it — but a
/// table that opens 200 columns wide has answered no question.
pub const SHOWN_SHARE: f64 = 0.2;

/// What a value is, as one bit each so a column's whole history fits in a
/// byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Null,
    Bool,
    Int,
    Float,
    Str,
    /// An object or an array: kept as the file's own bytes, for `K`.
    Nested,
}

impl Kind {
    const fn bit(self) -> u8 {
        match self {
            Kind::Null => 1,
            Kind::Bool => 2,
            Kind::Int => 4,
            Kind::Float => 8,
            Kind::Str => 16,
            Kind::Nested => 32,
        }
    }
}

/// One key, and everything the discovery pass learned about it.
#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    /// Records this key appeared in.
    pub rows: usize,
    /// Every [`Kind`] seen under it, as bits.
    kinds: u8,
    /// Which record it was last counted in, so a key repeated within one
    /// record does not count twice.
    last_row: usize,
}

impl Field {
    /// The column type this key's history allows.
    ///
    /// A key that only ever held whole numbers is an integer column and can be
    /// compared with `>`; one that ever held an object is text, because what
    /// it carries is a document. This is the hybrid the issue argued for: a
    /// typed `status` and a `payload` that `K` can open.
    pub fn dtype(&self) -> DataType {
        let has = |kind: Kind| self.kinds & kind.bit() != 0;
        if has(Kind::Nested) || has(Kind::Str) {
            DataType::String
        } else if has(Kind::Float) {
            DataType::Float64
        } else if has(Kind::Int) {
            DataType::Int64
        } else if has(Kind::Bool) {
            DataType::Boolean
        } else {
            // Only ever null, or never seen with a value at all.
            DataType::String
        }
    }
}

/// What the discovery pass found.
#[derive(Debug, Default)]
pub struct Fields {
    fields: Vec<Field>,
    pub rows: usize,
    /// Lines that were not a JSON object. They are still rows — the row count
    /// is the file's lines, and skipping one would put every row number after
    /// it out by one — they simply have no fields.
    pub malformed: usize,
    /// Keys past [`MAX_COLUMNS`], which are in the file but not in the table.
    pub dropped: usize,
}

impl Fields {
    /// The columns, most common first.
    ///
    /// Frequency and not first sight: the keys every record carries are the
    /// ones the table is about, and in a log the rare ones tend to arrive
    /// first, on whichever line happened to be an error. Ties keep the order
    /// they were seen in, so a record's own shape survives where counts do not
    /// separate them.
    pub fn columns(&self) -> Vec<&Field> {
        let mut ordered: Vec<(usize, &Field)> = self.fields.iter().enumerate().collect();
        ordered.sort_by(|(ai, a), (bi, b)| b.rows.cmp(&a.rows).then(ai.cmp(bi)));
        ordered
            .into_iter()
            .map(|(_, field)| field)
            .take(MAX_COLUMNS)
            .collect()
    }

    pub fn schema(&self) -> SchemaRef {
        let mut schema = Schema::default();
        for field in self.columns() {
            schema.with_column(field.name.as_str().into(), field.dtype());
        }
        Arc::new(schema)
    }

    /// Where each of [`Fields::columns`] lives in a record. A discovered key
    /// is one step down; `:expand` builds longer ones by prefixing these with
    /// the column it opened.
    pub fn paths(&self) -> Vec<KeyPath> {
        self.columns()
            .iter()
            .map(|field| vec![field.name.clone()])
            .collect()
    }

    /// What opening the file turned up that is worth saying once: keys that
    /// did not fit the table, lines that were not records. `None` when there
    /// is nothing to report, which is the ordinary case.
    ///
    /// Said out loud for the same reason a truncated filter reads
    /// `(first n)`: a table that quietly leaves something out reads as the
    /// whole answer.
    pub fn notes(&self) -> Option<String> {
        let mut said = Vec::new();
        if let Some(shown) = self.shown() {
            said.push(format!(
                "{} of {} keys shown (:reset select for all)",
                shown.len(),
                self.columns().len()
            ));
        }
        if self.dropped > 0 {
            said.push(format!(
                "{} keys past the {MAX_COLUMNS} column cap",
                self.dropped
            ));
        }
        if self.malformed > 0 {
            said.push(format!(
                "{} line{} not a JSON object",
                self.malformed,
                if self.malformed == 1 { "" } else { "s" }
            ));
        }
        (!said.is_empty()).then(|| said.join("; "))
    }

    /// Which columns to show at first, or `None` to show them all.
    ///
    /// `None` rather than "all of them" so the view is left alone in the
    /// ordinary case: a `:select` in the status bar for a table that is not
    /// hiding anything would be noise.
    pub fn shown(&self) -> Option<Vec<usize>> {
        let columns = self.columns();
        let floor = (self.rows as f64 * SHOWN_SHARE).ceil() as usize;
        let shown: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, field)| field.rows >= floor.max(1))
            .map(|(at, _)| at)
            .collect();
        (shown.len() < columns.len() && !shown.is_empty()).then_some(shown)
    }
}

/// Discovery, fed one byte at a time by the index scan.
///
/// Byte-fed because that is how the index reads the file, and the two must see
/// the same records: a scan that split lines differently from the index would
/// name its keys against the wrong rows.
#[derive(Default)]
pub struct Scan {
    line: Vec<u8>,
    at: HashMap<Vec<u8>, usize>,
    found: Fields,
    /// What to descend to before looking for keys. Empty for the record
    /// itself, which is how a file is opened; set by `:expand`, which asks
    /// the same question one level in.
    under: KeyPath,
    /// Records that actually had a document at `under`, which is what bounds
    /// a sample: a key on one line in a thousand needs a thousand lines read
    /// before there is anything to say about it.
    carrying: usize,
}

impl Scan {
    pub fn new() -> Self {
        Self::default()
    }

    /// A scan of what is *inside* one column, for `:expand`.
    pub fn under(path: KeyPath) -> Self {
        Self {
            under: path,
            ..Self::default()
        }
    }

    pub fn carrying(&self) -> usize {
        self.carrying
    }

    #[inline]
    pub fn byte(&mut self, byte: u8) {
        if byte == b'\n' {
            self.record();
        } else {
            self.line.push(byte);
        }
    }

    /// The last line, when the file did not end with a newline.
    pub fn finish(mut self) -> Fields {
        if !self.line.is_empty() {
            self.record();
        }
        self.found
    }

    fn record(&mut self) {
        let row = self.found.rows;
        self.found.rows += 1;

        let line = std::mem::take(&mut self.line);
        let mut fields = Vec::new();
        if !fields_of(trim_cr(&line), &mut fields) {
            self.found.malformed += 1;
            self.line = line;
            self.line.clear();
            return;
        }

        // Looking one level in: the record's own keys are not the answer, the
        // keys of the document at `under` are. A record that does not carry it
        // is not malformed, it simply has nothing to say here.
        if !self.under.is_empty() {
            let inner = fields
                .iter()
                .find(|(name, _, _)| *name == self.under[0].as_bytes())
                .and_then(|(_, kind, raw)| descend(*kind, raw, &self.under[1..]))
                .filter(|(kind, _)| *kind == Kind::Nested)
                .map(|(_, raw)| raw);
            fields.clear();
            match inner.filter(|raw| fields_of(raw, &mut fields)) {
                Some(_) => self.carrying += 1,
                None => {
                    self.line = line;
                    self.line.clear();
                    return;
                }
            }
        }

        for (name, kind, _) in fields {
            match self.at.get(name) {
                Some(&at) => {
                    let field = &mut self.found.fields[at];
                    field.kinds |= kind.bit();
                    // A key repeated inside one record is still one record.
                    if field.last_row != row {
                        field.last_row = row;
                        field.rows += 1;
                    }
                }
                None => {
                    if self.found.fields.len() >= MAX_COLUMNS {
                        self.found.dropped += 1;
                        continue;
                    }
                    self.at.insert(name.to_vec(), self.found.fields.len());
                    self.found.fields.push(Field {
                        name: String::from_utf8_lossy(name).into_owned(),
                        rows: 1,
                        kinds: kind.bit(),
                        last_row: row,
                    });
                }
            }
        }

        // Keep the allocation, drop the contents.
        self.line = line;
        self.line.clear();
    }
}

/// Records to look inside before answering what a column holds.
///
/// DuckDB's `sample_size` default, and for its reason: the answer wanted is
/// the shape of a document, and the shape is usually the same by the twentieth
/// record, let alone the twenty-thousandth. Counted in records that *carry*
/// the column, not lines read, so a key on one line in a thousand is still
/// sampled properly.
pub const SAMPLE_RECORDS: usize = 20_480;

/// And a hard stop on the reading, for a key so rare the count above would
/// mean walking a whole large file to find it. `:expand` runs in front of the
/// user, so it has to come back.
pub const SAMPLE_BYTES: u64 = 256 << 20;

/// What a sample found, and whether it saw the whole file.
pub struct Sample {
    pub fields: Fields,
    /// The scan reached the end of the file, so these keys are all of them.
    /// False means a bound was hit and the answer is what was in reach.
    pub complete: bool,
}

/// The keys of the documents inside one column.
///
/// A second pass over the file, unlike the key discovery that happens when a
/// file opens — that one rides along with the row index, which is already
/// reading every byte, and this one has nothing to ride. So it is bounded,
/// and says when a bound is what stopped it.
pub fn sample_under(file: &Path, under: &KeyPath) -> Result<Sample> {
    let mut scan = Scan::under(under.clone());
    let mut reader = BufReader::with_capacity(1 << 20, File::open(file)?);
    let mut read = 0u64;
    let complete = loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break true;
        }
        let taken = chunk.len();
        for &byte in chunk {
            scan.byte(byte);
        }
        read += taken as u64;
        reader.consume(taken);
        if scan.carrying() >= SAMPLE_RECORDS || read >= SAMPLE_BYTES {
            break false;
        }
    };
    Ok(Sample {
        fields: scan.finish(),
        complete,
    })
}

/// Where the value at `path` lives in a record, as a byte range.
///
/// What the writer splices over. The parse that finds it is the same one the
/// reader uses, so the bytes a cell was read from are exactly the bytes its
/// edit replaces — nothing else in the record is touched, which is the promise
/// `data/writer.rs` already makes for a delimited field.
pub fn locate(line: &[u8], path: &KeyPath) -> Option<Range<usize>> {
    let mut fields = Vec::new();
    if !fields_of(line, &mut fields) {
        return None;
    }
    let key = path.first()?;
    let (_, kind, raw) = fields.iter().find(|(name, _, _)| *name == key.as_bytes())?;
    let (_, raw) = descend(*kind, raw, &path[1..])?;
    Some(span_of(line, raw))
}

/// Where a key that the record does not have would go, and whether it would
/// need a comma in front of it.
///
/// The closing brace of the record, which is where a new member goes: a
/// document is not an ordered thing to a reader, and the end is the only place
/// that does not disturb what is already written.
pub fn insert_point(line: &[u8]) -> Option<(usize, bool)> {
    let mut fields = Vec::new();
    if !fields_of(line, &mut fields) {
        return None;
    }
    let close = line.iter().rposition(|&byte| byte == b'}')?;
    Some((close, !fields.is_empty()))
}

/// Where `inner` sits inside `outer`.
///
/// The parse hands back slices of the record rather than offsets into it, and
/// this turns one into the other. Sound because every slice it is given came
/// out of `outer` — a subslice's address is an offset from the start of what
/// it was cut from.
fn span_of(outer: &[u8], inner: &[u8]) -> Range<usize> {
    let at = inner.as_ptr() as usize - outer.as_ptr() as usize;
    at..at + inner.len()
}

/// Whether `text` is a JSON number, so it can be written into a record
/// without quotes. The one definition of that, shared with the reader.
pub fn is_number(text: &str) -> bool {
    matches!(number_end(text.as_bytes(), 0), Some((_, end)) if end == text.len())
}

/// A page of records, as the columns `wanted` names.
///
/// `bytes` is whole lines: the index only ever hands out spans that begin and
/// end at a record boundary. That span starts at the checkpoint before the
/// page, which can be a whole stride of records earlier, so `skip` and `take`
/// name the window actually wanted — parsing the rest of the span and
/// throwing it away costs a millisecond a keypress on a page deep in a file.
pub fn page(
    bytes: &[u8],
    schema: &SchemaRef,
    paths: &[KeyPath],
    wanted: &[usize],
    skip: usize,
    take: usize,
) -> Result<DataFrame> {
    let picked: Vec<(&str, &DataType, &KeyPath)> = wanted
        .iter()
        .filter_map(|&at| Some((schema.get_at_index(at)?, paths.get(at)?)))
        .map(|((name, dtype), path)| (name.as_str(), dtype, path))
        .collect();
    // Grouped by the key a column *starts* at, so one walk over a record's
    // fields still serves every column — an expanded one included, which
    // simply carries on down from the field it found.
    let mut column_at: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (at, (_, _, path)) in picked.iter().enumerate() {
        if let Some(first) = path.first() {
            column_at.entry(first.as_bytes()).or_default().push(at);
        }
    }

    let mut lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
    // A file's final newline closes the last record; it does not open another.
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    let lines = &lines[skip.min(lines.len())..];
    let lines = &lines[..take.min(lines.len())];

    let mut values: Vec<Vec<Option<String>>> = vec![Vec::with_capacity(lines.len()); picked.len()];
    let mut fields = Vec::new();
    for line in lines {
        for column in values.iter_mut() {
            column.push(None);
        }
        fields.clear();
        if !fields_of(trim_cr(line), &mut fields) {
            continue;
        }
        for (name, kind, raw) in &fields {
            let Some(columns) = column_at.get(name) else {
                continue;
            };
            for &at in columns {
                let path = picked[at].2;
                let Some((kind, raw)) = descend(*kind, raw, &path[1..]) else {
                    continue;
                };
                let last = values[at].len() - 1;
                values[at][last] = text(kind, raw);
            }
        }
    }

    let columns: Vec<Column> = picked
        .iter()
        .zip(values)
        .map(|((name, dtype, _), values)| column(name, dtype, values))
        .collect();
    let height = columns.first().map_or(0, |c| c.len());
    Ok(DataFrame::new(height, columns)?)
}

/// Where a column's value lives in a record: `["err"]` for a key, or
/// `["err", "code"]` for one lifted out of a document by `:expand`.
///
/// A path rather than a dotted name, because a JSON key may itself contain a
/// dot and splitting `err.code` would then be a guess about which of the two
/// it was. The name is for reading; this is for finding.
pub type KeyPath = Vec<String>;

/// Follow `rest` down from a value already found, or `None` where the record
/// does not go that deep.
fn descend<'a>(kind: Kind, raw: &'a [u8], rest: &[String]) -> Option<(Kind, &'a [u8])> {
    let Some(key) = rest.first() else {
        return Some((kind, raw));
    };
    if kind != Kind::Nested {
        return None;
    }
    let mut inner = Vec::new();
    if !fields_of(raw, &mut inner) {
        return None;
    }
    let (_, kind, raw) = inner.iter().find(|(name, _, _)| *name == key.as_bytes())?;
    descend(*kind, raw, &rest[1..])
}

/// What a cell holds.
///
/// A string arrives without its quotes and with its escapes resolved, since
/// that is the value; everything else is the file's own bytes, so a nested
/// value is still the document it was and `K` can open it.
fn text(kind: Kind, raw: &[u8]) -> Option<String> {
    match kind {
        Kind::Null => None,
        Kind::Str => Some(unescape(&String::from_utf8_lossy(
            &raw[1..raw.len().saturating_sub(1).max(1)],
        ))),
        _ => Some(String::from_utf8_lossy(raw).into_owned()),
    }
}

/// One column, typed as the discovery pass said it could be.
///
/// A value that will not parse as the column's type becomes a null rather than
/// failing the page: the type came from a scan of the whole file, so this is a
/// rare disagreement, and a page that refuses to draw is worse than a cell
/// that says it has nothing.
fn column(name: &str, dtype: &DataType, values: Vec<Option<String>>) -> Column {
    let name: PlSmallStr = name.into();
    match dtype {
        DataType::Int64 => Column::new(
            name,
            values
                .iter()
                .map(|v| v.as_ref().and_then(|v| v.parse::<i64>().ok()))
                .collect::<Vec<_>>(),
        ),
        DataType::Float64 => Column::new(
            name,
            values
                .iter()
                .map(|v| v.as_ref().and_then(|v| v.parse::<f64>().ok()))
                .collect::<Vec<_>>(),
        ),
        DataType::Boolean => Column::new(
            name,
            values
                .iter()
                .map(|v| match v.as_deref() {
                    Some("true") => Some(true),
                    Some("false") => Some(false),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        ),
        _ => Column::new(name, values),
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// The top-level fields of one record: `(key, what the value is, its bytes)`.
///
/// False when the line is not a JSON object — a blank line, a stack trace
/// someone let into the log, a bare array. The row still exists, it just has
/// no fields; dropping it would put every row number after it out by one, and
/// the row numbers are what the edit buffer and the filter are keyed by.
///
/// Only *top-level* keys become fields. Anything deeper stays inside its value
/// and is read with `K`: one rule, and it ties the column count to the shape
/// of a record rather than to its depth.
fn fields_of<'a>(line: &'a [u8], out: &mut Vec<(&'a [u8], Kind, &'a [u8])>) -> bool {
    let mut at = skip_space(line, 0);
    if line.get(at) != Some(&b'{') {
        return false;
    }
    at = skip_space(line, at + 1);
    if line.get(at) == Some(&b'}') {
        return skip_space(line, at + 1) == line.len();
    }
    loop {
        at = skip_space(line, at);
        let Some(key_end) = string_end(line, at) else {
            return false;
        };
        let key = &line[at + 1..key_end - 1];
        at = skip_space(line, key_end);
        if line.get(at) != Some(&b':') {
            return false;
        }
        at = skip_space(line, at + 1);
        let Some((kind, end)) = value_end(line, at) else {
            return false;
        };
        out.push((key, kind, &line[at..end]));
        at = skip_space(line, end);
        match line.get(at) {
            Some(b',') => at += 1,
            Some(b'}') => return skip_space(line, at + 1) == line.len(),
            _ => return false,
        }
    }
}

fn skip_space(line: &[u8], mut at: usize) -> usize {
    while matches!(line.get(at), Some(b' ' | b'\t' | b'\r')) {
        at += 1;
    }
    at
}

/// One past the closing quote of the string starting at `at`.
fn string_end(line: &[u8], at: usize) -> Option<usize> {
    if line.get(at) != Some(&b'"') {
        return None;
    }
    let mut i = at + 1;
    loop {
        match line.get(i)? {
            b'\\' => {
                line.get(i + 1)?;
                i += 2;
            }
            b'"' => return Some(i + 1),
            // A raw control character is not allowed in a JSON string. Letting
            // one through would make a line of prose with a brace in it read
            // as a record.
            byte if *byte < 0x20 => return None,
            _ => i += 1,
        }
    }
}

/// What the value at `at` is, and where it ends.
fn value_end(line: &[u8], at: usize) -> Option<(Kind, usize)> {
    match line.get(at)? {
        b'"' => string_end(line, at).map(|end| (Kind::Str, end)),
        b'{' | b'[' => nested_end(line, at).map(|end| (Kind::Nested, end)),
        b't' => line[at..]
            .starts_with(b"true")
            .then_some((Kind::Bool, at + 4)),
        b'f' => line[at..]
            .starts_with(b"false")
            .then_some((Kind::Bool, at + 5)),
        b'n' => line[at..]
            .starts_with(b"null")
            .then_some((Kind::Null, at + 4)),
        b'-' | b'0'..=b'9' => number_end(line, at),
        _ => None,
    }
}

/// The end of an object or array, counting depth and stepping over strings so
/// a brace inside one does not close it.
fn nested_end(line: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = at;
    while let Some(byte) = line.get(i) {
        match byte {
            b'"' => {
                i = string_end(line, i)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Checked in the shape JSON allows, so `01` and `1.` are not numbers — and
/// told apart into whole and not, which is what decides whether the column can
/// be compared with `>`.
fn number_end(line: &[u8], at: usize) -> Option<(Kind, usize)> {
    let mut i = at;
    let mut kind = Kind::Int;
    if line.get(i) == Some(&b'-') {
        i += 1;
    }
    let whole = i;
    while matches!(line.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == whole || (i - whole > 1 && line[whole] == b'0') {
        return None;
    }
    if line.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while matches!(line.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == start {
            return None;
        }
        kind = Kind::Float;
    }
    if matches!(line.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(line.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while matches!(line.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == start {
            return None;
        }
        kind = Kind::Float;
    }
    Some((kind, i))
}

/// A JSON string's own text: escapes resolved, so a `\n` in a log message is a
/// line break the cell window can show as one.
fn unescape(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    // A lone surrogate half, which is not a character. Left as
                    // written rather than guessed at.
                    None => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Fields {
        let mut scan = Scan::new();
        for byte in text.bytes() {
            scan.byte(byte);
        }
        scan.finish()
    }

    fn names(fields: &Fields) -> Vec<&str> {
        fields
            .columns()
            .iter()
            .map(|field| field.name.as_str())
            .collect()
    }

    fn cell(df: &DataFrame, column: &str, row: usize) -> String {
        df.column(column)
            .unwrap()
            .get(row)
            .unwrap()
            .str_value()
            .to_string()
    }

    const LOG: &str = concat!(
        r#"{"ts":"2026-09-07T10:00:00Z","level":"info","msg":"started","port":8080}"#,
        "\n",
        r#"{"ts":"2026-09-07T10:00:01Z","level":"error","msg":"boom","err":{"code":500,"why":"bad"}}"#,
        "\n",
    );

    #[test]
    fn the_keys_are_the_columns_most_common_first() {
        let fields = scan(LOG);
        assert_eq!(fields.rows, 2);
        assert_eq!(names(&fields), ["ts", "level", "msg", "port", "err"]);
    }

    #[test]
    fn a_column_is_typed_by_everything_that_was_ever_in_it() {
        let fields = scan(concat!(
            r#"{"n":1,"f":1,"s":1,"b":true,"j":{"a":1}}"#,
            "\n",
            r#"{"n":2,"f":1.5,"s":"x","b":false,"j":[1]}"#,
            "\n"
        ));
        let by_name: HashMap<&str, DataType> = fields
            .columns()
            .iter()
            .map(|field| (field.name.as_str(), field.dtype()))
            .collect();
        assert_eq!(by_name["n"], DataType::Int64, "whole numbers throughout");
        assert_eq!(by_name["f"], DataType::Float64, "one of them was not");
        assert_eq!(by_name["s"], DataType::String, "a number and a string");
        assert_eq!(by_name["b"], DataType::Boolean);
        assert_eq!(
            by_name["j"],
            DataType::String,
            "a document, kept as its own text"
        );
    }

    #[test]
    fn a_nested_value_is_carried_through_as_the_files_own_bytes() {
        let fields = scan(LOG);
        let schema = fields.schema();
        let df = page(
            LOG.as_bytes(),
            &schema,
            &fields.paths(),
            &(0..schema.len()).collect::<Vec<_>>(),
            0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(
            cell(&df, "err", 1),
            r#"{"code":500,"why":"bad"}"#,
            "byte for byte, so `K` can open it"
        );
        assert!(
            crate::ui::json::reindent(&cell(&df, "err", 1)).is_some(),
            "and it really is a document"
        );
    }

    #[test]
    fn a_string_arrives_without_its_quotes_and_with_its_escapes_resolved() {
        let line = "{\"msg\":\"one\\ntwo \\\"quoted\\\" \\u00e9\"}\n";
        let fields = scan(line);
        let schema = fields.schema();
        let df = page(
            line.as_bytes(),
            &schema,
            &fields.paths(),
            &[0],
            0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(cell(&df, "msg", 0), "one\ntwo \"quoted\" é");
    }

    #[test]
    fn a_missing_key_is_a_null_and_not_a_shifted_row() {
        let fields = scan(LOG);
        let schema = fields.schema();
        let df = page(
            LOG.as_bytes(),
            &schema,
            &fields.paths(),
            &(0..schema.len()).collect::<Vec<_>>(),
            0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(df.height(), 2);
        assert_eq!(cell(&df, "port", 0), "8080");
        assert!(df.column("port").unwrap().get(1).unwrap().is_null());
    }

    #[test]
    fn a_line_that_is_not_a_record_is_still_a_row() {
        // Row numbers are the file's lines. Skipping one would put every row
        // after it out by one, and those numbers key the filter and the
        // overlay.
        let text = concat!(r#"{"a":1}"#, "\n", "not json at all\n", r#"{"a":3}"#, "\n");
        let fields = scan(text);
        assert_eq!(fields.rows, 3);
        assert_eq!(fields.malformed, 1);

        let schema = fields.schema();
        let df = page(
            text.as_bytes(),
            &schema,
            &fields.paths(),
            &[0],
            0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(df.height(), 3);
        assert_eq!(cell(&df, "a", 0), "1");
        assert!(df.column("a").unwrap().get(1).unwrap().is_null());
        assert_eq!(cell(&df, "a", 2), "3");
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_record() {
        let text = "{\"msg\":\"a } and a { in prose\",\"n\":1}\n";
        let fields = scan(text);
        assert_eq!(names(&fields), ["msg", "n"]);
    }

    #[test]
    fn the_rare_keys_are_available_but_not_shown() {
        // Nine records with `a`, one of which also has `rare`.
        let mut text = String::new();
        for i in 0..9 {
            text.push_str(&format!("{{\"a\":{i}}}\n"));
        }
        text.push_str("{\"a\":9,\"rare\":1}\n");
        let fields = scan(&text);
        assert_eq!(names(&fields), ["a", "rare"]);
        assert_eq!(
            fields.shown(),
            Some(vec![0]),
            "one in ten is below the share"
        );
    }

    #[test]
    fn nothing_is_hidden_when_every_key_is_common() {
        let fields = scan(concat!(r#"{"a":1,"b":2}"#, "\n", r#"{"a":3,"b":4}"#, "\n"));
        assert_eq!(fields.shown(), None, "so the view is left alone");
    }

    #[test]
    fn a_key_repeated_in_one_record_counts_once() {
        let fields = scan(concat!(r#"{"a":1,"a":2}"#, "\n", r#"{"b":1}"#, "\n"));
        let counts: HashMap<&str, usize> = fields
            .columns()
            .iter()
            .map(|field| (field.name.as_str(), field.rows))
            .collect();
        assert_eq!(counts["a"], 1);
    }

    #[test]
    fn a_file_that_does_not_end_in_a_newline_still_ends_its_record() {
        let fields = scan(r#"{"a":1}"#);
        assert_eq!(fields.rows, 1);
        assert_eq!(names(&fields), ["a"]);
    }

    #[test]
    fn numbers_are_read_in_the_shape_json_allows() {
        assert_eq!(number_end(b"01", 0), None, "no leading zero");
        assert_eq!(number_end(b"1.", 0), None, "no bare point");
        assert_eq!(number_end(b"1e3", 0), Some((Kind::Float, 3)));
        assert_eq!(number_end(b"-0.5,", 0), Some((Kind::Float, 4)));
        assert_eq!(number_end(b"42}", 0), Some((Kind::Int, 2)));
    }

    /// A span starts at the checkpoint before the page, which can be a whole
    /// stride of records earlier. Only the window asked for is built.
    #[test]
    fn a_page_builds_only_the_window_it_was_asked_for() {
        let fields = scan(LOG);
        let schema = fields.schema();
        let df = page(LOG.as_bytes(), &schema, &fields.paths(), &[0], 1, 1).unwrap();
        assert_eq!(df.height(), 1);
        assert_eq!(cell(&df, "ts", 0), "2026-09-07T10:00:01Z");
    }

    #[test]
    fn what_is_inside_a_column_is_found_by_looking_under_it() {
        let path = std::env::temp_dir().join("plv-jsonl-sample.jsonl");
        std::fs::write(&path, LOG).unwrap();
        let sample = sample_under(&path, &vec!["err".to_string()]).unwrap();
        assert!(sample.complete, "the whole file fits inside the bounds");
        assert_eq!(
            sample
                .fields
                .columns()
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["code", "why"]
        );
        assert_eq!(sample.fields.columns()[0].dtype(), DataType::Int64);
    }

    #[test]
    fn a_column_can_be_read_from_inside_another() {
        let fields = scan(LOG);
        let mut schema = (*fields.schema()).clone();
        schema.with_column("err.code".into(), DataType::Int64);
        let schema = Arc::new(schema);
        let mut paths = fields.paths();
        paths.push(vec!["err".to_string(), "code".to_string()]);

        let at = schema.len() - 1;
        let df = page(LOG.as_bytes(), &schema, &paths, &[at], 0, usize::MAX).unwrap();
        assert!(
            df.column("err.code").unwrap().get(0).unwrap().is_null(),
            "the record with no err has nothing there"
        );
        assert_eq!(cell(&df, "err.code", 1), "500");
    }

    /// A key may itself contain a dot, so a column's name is not a route to
    /// its value — the path is.
    #[test]
    fn a_dotted_key_is_not_mistaken_for_a_path() {
        let line = r#"{"a.b":1,"a":{"b":2}}"#.to_string() + "\n";
        let fields = scan(&line);
        let schema = fields.schema();
        let paths = fields.paths();
        let df = page(line.as_bytes(), &schema, &paths, &[0], 0, usize::MAX).unwrap();
        assert_eq!(cell(&df, "a.b", 0), "1", "the key, not the route");
    }

    #[test]
    fn a_page_only_builds_the_columns_it_was_asked_for() {
        let fields = scan(LOG);
        let schema = fields.schema();
        let df = page(
            LOG.as_bytes(),
            &schema,
            &fields.paths(),
            &[1],
            0,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(df.get_column_names(), ["level"]);
        assert_eq!(cell(&df, "level", 1), "error");
    }
}
