//! The view language: what `:select`, `:hide`, `:filter` and `:sort` mean.
//!
//! Parsing and validation only — nothing here touches a `LazyFrame`. A command
//! is turned into resolved, checked state that the data layer can act on, and
//! anything wrong with it is reported here, against the word that caused it.
//!
//! **Why validate now rather than at `collect()`.** Polars is lazy, so a
//! comparison against the wrong type does not fail where it was written; it
//! fails later, inside a collect, as a query-planner error naming nodes the
//! user never typed. The schema is right here when the line is entered, so the
//! answer is too.
//!
//! **Why state rather than a pipeline.** Each command replaces its own slot:
//! `:select a b` then `:select c` shows `c`, rather than trying to select `c`
//! from a frame already narrowed to `a b` — which would be an error rather than
//! a refinement. `Store::sort` already works this way.

use std::fmt::Write as _;
use std::ops::Range;

use polars::prelude::{DataType, Schema};
use regex::Regex;

/// A parsed, schema-checked command. Columns are indices: names are resolved
/// once, here, so nothing downstream has to look them up again — and duplicate
/// names cannot quietly resolve to the wrong column.
#[derive(Debug, Clone)]
pub enum Command {
    Select(Vec<usize>),
    Hide(Vec<usize>),
    Filter(Filter),
    Sort(Vec<(usize, bool)>),
    Reset(Option<Slot>),
}

/// Which part of the view a command addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Select,
    Filter,
    Sort,
}

/// Conditions joined by `and`.
///
/// No `or` and no parentheses: precedence is the thing that cannot be added
/// later without changing what already-written commands mean, so the grammar
/// stays flat until there is a reason for it not to be.
#[derive(Debug, Clone)]
pub struct Filter {
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone)]
pub struct Condition {
    pub column: usize,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `~` — regex, matched against the column rendered as text, exactly as
    /// `/` search does.
    Matches,
    NotMatches,
}

impl Op {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "=" | "==" => Op::Eq,
            "!=" => Op::Ne,
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "~" => Op::Matches,
            "!~" => Op::NotMatches,
            _ => return None,
        })
    }

    fn is_regex(self) -> bool {
        matches!(self, Op::Matches | Op::NotMatches)
    }

    fn is_ordering(self) -> bool {
        matches!(self, Op::Lt | Op::Le | Op::Gt | Op::Ge)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Matches => "~",
            Op::NotMatches => "!~",
        }
    }
}

/// A literal, already resolved against the column it will be compared with.
#[derive(Debug, Clone)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Text(String),
    Pattern(Regex),
    /// An empty literal. plv treats an absent field and an empty one as the
    /// same thing everywhere else — it renders both as `·` and writes both back
    /// as empty — so `= ""` asks for the cells with nothing in them.
    Empty,
}

/// The active view. Each command replaces one slot; nothing accumulates.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Columns to show, in display order. `None` means all of them.
    pub select: Option<Vec<usize>>,
    pub filter: Option<Filter>,
    /// Sort keys in priority order, as `(column, ascending)` — the same shape
    /// `Store::sort` already uses.
    pub sort: Vec<(usize, bool)>,
}

impl View {
    pub fn is_empty(&self) -> bool {
        self.select.is_none() && self.filter.is_none() && self.sort.is_empty()
    }

    /// The columns on show, in order, given how many the file has.
    pub fn columns(&self, total: usize) -> Vec<usize> {
        match &self.select {
            Some(cols) => cols.clone(),
            None => (0..total).collect(),
        }
    }

    /// Fold a command into the view.
    ///
    /// Refuses only what would leave nothing to look at; everything else was
    /// already checked when the line was parsed.
    pub fn apply(&mut self, command: Command, total_columns: usize) -> Result<(), String> {
        match command {
            Command::Select(cols) => self.select = Some(cols),
            Command::Hide(hidden) => {
                // Hiding narrows what is on show, so it writes to the same slot
                // `:select` does — and a later `:select` replaces the lot.
                let kept: Vec<usize> = self
                    .columns(total_columns)
                    .into_iter()
                    .filter(|col| !hidden.contains(col))
                    .collect();
                if kept.is_empty() {
                    return Err("that would hide every column".to_string());
                }
                self.select = Some(kept);
            }
            Command::Filter(filter) => {
                // A filter is resolved to a set of source rows; a sort puts the
                // rows in an order those numbers no longer describe. Carrying
                // both would mean re-deriving the set on every sort, so for now
                // the two are exclusive and say so.
                if !self.sort.is_empty() {
                    return Err("cannot filter a sorted view — :sort clears it".to_string());
                }
                self.filter = Some(filter);
            }
            Command::Sort(keys) => {
                if self.filter.is_some() && !keys.is_empty() {
                    return Err("cannot sort a filtered view — :filter clears it".to_string());
                }
                self.sort = keys;
            }
            Command::Reset(None) => *self = Self::default(),
            Command::Reset(Some(Slot::Select)) => self.select = None,
            Command::Reset(Some(Slot::Filter)) => self.filter = None,
            Command::Reset(Some(Slot::Sort)) => self.sort.clear(),
        }
        Ok(())
    }

    /// A one-line summary for the status bar, or `None` when nothing is set.
    pub fn describe(&self, schema: &Schema) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let name = |index: usize| {
            schema
                .get_at_index(index)
                .map_or_else(|| format!("#{index}"), |(name, _)| name.to_string())
        };
        let mut parts = Vec::new();
        if let Some(cols) = &self.select {
            parts.push(format!("select {}/{}", cols.len(), schema.len()));
        }
        if let Some(filter) = &self.filter {
            let mut text = String::new();
            for (i, condition) in filter.conditions.iter().enumerate() {
                if i > 0 {
                    text.push_str(" and ");
                }
                let _ = write!(
                    text,
                    "{} {} {}",
                    name(condition.column),
                    condition.op.as_str(),
                    condition.value.display()
                );
            }
            parts.push(text);
        }
        if !self.sort.is_empty() {
            let keys: Vec<String> = self
                .sort
                .iter()
                .map(|&(col, asc)| format!("{}{}", name(col), if asc { "" } else { "-" }))
                .collect();
            parts.push(format!("sort {}", keys.join(" ")));
        }
        Some(parts.join("  "))
    }
}

impl Value {
    fn display(&self) -> String {
        match self {
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Text(s) => s.clone(),
            Value::Pattern(r) => r.as_str().to_string(),
            Value::Empty => "\"\"".to_string(),
        }
    }
}

/// What went wrong, and which part of the line it was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Byte range in the input the message refers to.
    pub span: Range<usize>,
}

impl ParseError {
    fn new(message: impl Into<String>, span: Range<usize>) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }

    /// The offending span underlined, for a prompt with room for two lines.
    pub fn caret(&self, line: &str) -> String {
        let start = line[..self.span.start.min(line.len())].chars().count();
        let width = line
            .get(self.span.clone())
            .map_or(1, |text| text.chars().count().max(1));
        format!("{}{}", " ".repeat(start), "^".repeat(width))
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One word of the command line, and where it came from.
struct Token<'a> {
    text: &'a str,
    span: Range<usize>,
    /// Written inside quotes, so it is a literal even if it looks like a verb,
    /// an operator, or a number.
    quoted: bool,
}

/// Split on whitespace, keeping `"quoted runs"` together.
///
/// No escapes: a quoted run ends at the next `"`. Quoting exists so that column
/// names with spaces — or one that happens to end in `-` — can be written at
/// all, not as a general string syntax.
fn tokenize(line: &str) -> Result<Vec<Token<'_>>, ParseError> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i] == b'"' {
            let open = i;
            let Some(close) = line[i + 1..].find('"').map(|n| i + 1 + n) else {
                return Err(ParseError::new("unclosed quote", open..line.len()));
            };
            tokens.push(Token {
                text: &line[i + 1..close],
                span: open..close + 1,
                quoted: true,
            });
            i = close + 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' {
            i += 1;
        }
        tokens.push(Token {
            text: &line[start..i],
            span: start..i,
            quoted: false,
        });
    }
    Ok(tokens)
}

/// Parse one `:` line into a checked command.
///
/// The caller handles its own verbs — `w`, `q` and friends — before reaching
/// here; an unrecognised word is reported as an unknown command.
pub fn parse(line: &str, schema: &Schema) -> Result<Command, ParseError> {
    let tokens = tokenize(line)?;
    let Some((verb, args)) = tokens.split_first() else {
        return Err(ParseError::new("nothing to do", 0..0));
    };

    // A verb with nothing after it undoes itself. `:select` is where the hand
    // goes to put the columns back, and no other reading of it is useful —
    // selecting nothing is refused anyway.
    if args.is_empty() {
        if let Some(slot) = slot_of(verb.text) {
            return Ok(Command::Reset(Some(slot)));
        }
    }

    match verb.text {
        // `*` is the same thing said out loud, for anyone who reaches for SQL.
        "select" if is_star(args) => Ok(Command::Reset(Some(Slot::Select))),
        "select" => Ok(Command::Select(columns(args, schema, verb, "select")?)),
        "hide" => Ok(Command::Hide(columns(args, schema, verb, "hide")?)),
        "sort" => parse_sort(args, schema, verb),
        "filter" => parse_filter(args, schema, verb),
        "reset" => parse_reset(args),
        other => Err(ParseError::new(
            format!("not a command: :{other}"),
            verb.span.clone(),
        )),
    }
}

/// The slot a verb writes to, for the bare form that clears it. `hide` narrows
/// what `select` shows, so it clears the same slot.
fn slot_of(verb: &str) -> Option<Slot> {
    Some(match verb {
        "select" | "hide" => Slot::Select,
        "filter" => Slot::Filter,
        "sort" => Slot::Sort,
        _ => return None,
    })
}

fn is_star(args: &[Token<'_>]) -> bool {
    matches!(args, [only] if !only.quoted && only.text == "*")
}

/// A non-empty list of columns, each resolved to its index.
fn columns(
    args: &[Token<'_>],
    schema: &Schema,
    verb: &Token<'_>,
    what: &str,
) -> Result<Vec<usize>, ParseError> {
    if args.is_empty() {
        return Err(ParseError::new(
            format!(":{what} needs at least one column"),
            verb.span.clone(),
        ));
    }
    args.iter().map(|arg| resolve_column(arg, schema)).collect()
}

fn parse_sort(
    args: &[Token<'_>],
    schema: &Schema,
    verb: &Token<'_>,
) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::new(
            ":sort needs at least one column",
            verb.span.clone(),
        ));
    }
    let keys = args
        .iter()
        .map(|arg| {
            // A trailing `-` reverses the key. A column whose name really ends
            // in one can be written in quotes.
            let descending = !arg.quoted && arg.text.len() > 1 && arg.text.ends_with('-');
            let bare = Token {
                text: if descending {
                    &arg.text[..arg.text.len() - 1]
                } else {
                    arg.text
                },
                span: arg.span.clone(),
                quoted: arg.quoted,
            };
            resolve_column(&bare, schema).map(|col| (col, !descending))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Command::Sort(keys))
}

fn parse_reset(args: &[Token<'_>]) -> Result<Command, ParseError> {
    let Some(slot) = args.first() else {
        return Ok(Command::Reset(None));
    };
    let named = match slot.text {
        "select" | "hide" => Slot::Select,
        "filter" => Slot::Filter,
        "sort" => Slot::Sort,
        other => {
            return Err(ParseError::new(
                format!("nothing called {other:?} to reset — try select, filter or sort"),
                slot.span.clone(),
            ));
        }
    };
    Ok(Command::Reset(Some(named)))
}

/// `<col> <op> <value>` runs joined by `and`.
fn parse_filter(
    args: &[Token<'_>],
    schema: &Schema,
    verb: &Token<'_>,
) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::new(
            ":filter needs a condition, as `column op value`",
            verb.span.clone(),
        ));
    }

    let mut conditions = Vec::new();
    let mut rest = args;
    loop {
        let (condition, tail) = parse_condition(rest, schema)?;
        conditions.push(condition);
        match tail.split_first() {
            None => break,
            Some((joiner, after)) if !joiner.quoted && joiner.text == "and" => {
                if after.is_empty() {
                    return Err(ParseError::new(
                        "`and` needs another condition after it",
                        joiner.span.clone(),
                    ));
                }
                rest = after;
            }
            Some((extra, _)) => {
                return Err(ParseError::new(
                    format!(
                        "expected `and` or the end of the line, found {:?}",
                        extra.text
                    ),
                    extra.span.clone(),
                ));
            }
        }
    }
    Ok(Command::Filter(Filter { conditions }))
}

fn parse_condition<'a, 'b>(
    args: &'b [Token<'a>],
    schema: &Schema,
) -> Result<(Condition, &'b [Token<'a>]), ParseError> {
    let [column, op, value, rest @ ..] = args else {
        let span = args
            .first()
            .map_or(0..0, |t| t.span.start..args[args.len() - 1].span.end);
        return Err(ParseError::new(
            "a condition reads `column op value`, as in `count > 10`",
            span,
        ));
    };

    let index = resolve_column(column, schema)?;
    let Some(operator) = (if op.quoted { None } else { Op::parse(op.text) }) else {
        return Err(ParseError::new(
            format!(
                "{:?} is not a comparison — use one of = != < <= > >= ~ !~",
                op.text
            ),
            op.span.clone(),
        ));
    };
    let (_, dtype) = schema
        .get_at_index(index)
        .expect("resolve_column returned a real index");
    let resolved = resolve_value(operator, dtype, value, column.text)?;

    Ok((
        Condition {
            column: index,
            op: operator,
            value: resolved,
        },
        rest,
    ))
}

/// Check the literal against the column it will be compared with.
///
/// This is the whole point of parsing with the schema in hand: `count > abc` is
/// answered here, naming the column and the type, rather than surfacing later
/// as a cast failure from inside a query plan.
fn resolve_value(
    op: Op,
    dtype: &DataType,
    token: &Token<'_>,
    column: &str,
) -> Result<Value, ParseError> {
    if op.is_regex() {
        // Regex reads every column as text, as `/` search does, so any column
        // can take one.
        return Regex::new(token.text).map(Value::Pattern).map_err(|e| {
            ParseError::new(
                format!("not a regex: {}", first_line(&e.to_string())),
                token.span.clone(),
            )
        });
    }
    if token.text.is_empty() {
        return Ok(Value::Empty);
    }

    if dtype.is_integer() || dtype.is_float() {
        return token.text.parse::<f64>().map(Value::Number).map_err(|_| {
            ParseError::new(
                format!(
                    "{:?} is not a number, and column {column:?} is {dtype}",
                    token.text
                ),
                token.span.clone(),
            )
        });
    }
    if matches!(dtype, DataType::Boolean) {
        if op.is_ordering() {
            return Err(ParseError::new(
                format!("column {column:?} is a boolean, so it cannot be ordered"),
                token.span.clone(),
            ));
        }
        return match token.text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            other => Err(ParseError::new(
                format!("{other:?} is not true or false, and column {column:?} is a boolean"),
                token.span.clone(),
            )),
        };
    }
    Ok(Value::Text(token.text.to_string()))
}

fn resolve_column(token: &Token<'_>, schema: &Schema) -> Result<usize, ParseError> {
    let found: Vec<usize> = schema
        .iter_names()
        .enumerate()
        .filter(|(_, name)| name.as_str() == token.text)
        .map(|(index, _)| index)
        .collect();

    match found.as_slice() {
        [only] => Ok(*only),
        [] => Err(ParseError::new(
            unknown_column(token.text, schema),
            token.span.clone(),
        )),
        many => Err(ParseError::new(
            format!(
                "{:?} names {} columns — there is no way to say which",
                token.text,
                many.len()
            ),
            token.span.clone(),
        )),
    }
}

fn unknown_column(name: &str, schema: &Schema) -> String {
    let lower = name.to_lowercase();
    let hint = schema
        .iter_names()
        .find(|known| {
            let known = known.as_str().to_lowercase();
            known.starts_with(&lower) || known.contains(&lower)
        })
        .map(|known| format!(" — did you mean {:?}?", known.as_str()));
    format!("no column called {name:?}{}", hint.unwrap_or_default())
}

/// Regex errors are several lines with their own caret; the prompt has room
/// for the first.
fn first_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(message)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use polars::prelude::PlSmallStr;

    /// name:str  count:i64  ratio:f64  ok:bool  "release date":str  "odd-":str
    fn schema() -> Schema {
        Schema::from_iter([
            (PlSmallStr::from_static("name"), DataType::String),
            (PlSmallStr::from_static("count"), DataType::Int64),
            (PlSmallStr::from_static("ratio"), DataType::Float64),
            (PlSmallStr::from_static("ok"), DataType::Boolean),
            (PlSmallStr::from_static("release date"), DataType::String),
            (PlSmallStr::from_static("odd-"), DataType::String),
        ])
    }

    fn ok(line: &str) -> Command {
        parse(line, &schema()).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    fn err(line: &str) -> ParseError {
        parse(line, &schema()).expect_err(&format!("{line:?} should not parse"))
    }

    // ── columns ──────────────────────────────────────────────────────────

    #[test]
    fn select_resolves_names_to_indices() {
        let Command::Select(cols) = ok("select count name") else {
            panic!("not a select")
        };
        assert_eq!(cols, [1, 0], "order follows the command, not the file");
    }

    #[test]
    fn a_name_with_spaces_can_be_quoted() {
        let Command::Select(cols) = ok(r#"select "release date""#) else {
            panic!("not a select")
        };
        assert_eq!(cols, [4]);
    }

    #[test]
    fn an_unknown_column_is_named_and_a_near_miss_offered() {
        let e = err("select coutn");
        assert!(e.message.contains(r#"no column called "coutn""#), "{e}");

        let e = err("select rat");
        assert!(e.message.contains(r#"did you mean "ratio""#), "{e}");
    }

    #[test]
    fn the_error_points_at_the_word_that_caused_it() {
        let line = "select name nope";
        let e = parse(line, &schema()).unwrap_err();
        assert_eq!(&line[e.span.clone()], "nope");
        assert_eq!(e.caret(line), "            ^^^^");
    }

    #[test]
    fn a_bare_verb_clears_what_it_set() {
        assert!(matches!(ok("select"), Command::Reset(Some(Slot::Select))));
        assert!(matches!(ok("select *"), Command::Reset(Some(Slot::Select))));
        assert!(matches!(ok("hide"), Command::Reset(Some(Slot::Select))));
        assert!(matches!(ok("sort"), Command::Reset(Some(Slot::Sort))));
        assert!(matches!(ok("filter"), Command::Reset(Some(Slot::Filter))));

        // A column really called `*` is still reachable.
        assert!(err(r#"select "*""#).message.contains("no column"));
    }

    // ── sort ─────────────────────────────────────────────────────────────

    #[test]
    fn a_trailing_dash_reverses_a_sort_key() {
        let Command::Sort(keys) = ok("sort count- name") else {
            panic!("not a sort")
        };
        assert_eq!(keys, [(1, false), (0, true)]);
    }

    #[test]
    fn quoting_rescues_a_column_whose_name_ends_in_a_dash() {
        let Command::Sort(keys) = ok(r#"sort "odd-""#) else {
            panic!("not a sort")
        };
        assert_eq!(keys, [(5, true)], "quoted, so the dash is part of the name");

        // Unquoted it reads as descending, and there is no column `odd`.
        assert!(
            err("sort odd-")
                .message
                .contains(r#"no column called "odd""#)
        );
    }

    // ── filter ───────────────────────────────────────────────────────────

    #[test]
    fn a_condition_is_checked_against_the_column_it_compares() {
        let Command::Filter(f) = ok("filter count > 10") else {
            panic!("not a filter")
        };
        assert_eq!(f.conditions.len(), 1);
        assert_eq!(f.conditions[0].column, 1);
        assert_eq!(f.conditions[0].op, Op::Gt);
        assert!(matches!(f.conditions[0].value, Value::Number(n) if n == 10.0));
    }

    #[test]
    fn a_number_is_required_where_the_column_is_numeric() {
        let e = err("filter count > abc");
        assert!(e.message.contains("not a number"), "{e}");
        assert!(e.message.contains(r#"column "count" is i64"#), "{e}");
        assert_eq!(&"filter count > abc"[e.span.clone()], "abc");
    }

    #[test]
    fn a_text_column_takes_a_bare_word_that_looks_like_a_number() {
        let Command::Filter(f) = ok("filter name = 42") else {
            panic!("not a filter")
        };
        assert!(matches!(&f.conditions[0].value, Value::Text(t) if t == "42"));
    }

    #[test]
    fn a_boolean_column_takes_true_and_false_and_refuses_ordering() {
        let Command::Filter(f) = ok("filter ok = true") else {
            panic!("not a filter")
        };
        assert!(matches!(f.conditions[0].value, Value::Bool(true)));

        assert!(err("filter ok = yes").message.contains("not true or false"));
        assert!(
            err("filter ok > false")
                .message
                .contains("cannot be ordered")
        );
    }

    #[test]
    fn a_regex_reads_any_column_as_text() {
        // Numeric columns included: `/` search does the same.
        let Command::Filter(f) = ok("filter count ~ ^1") else {
            panic!("not a filter")
        };
        assert!(matches!(&f.conditions[0].value, Value::Pattern(r) if r.as_str() == "^1"));

        let e = err("filter name ~ [");
        assert!(e.message.starts_with("not a regex:"), "{e}");
        assert_eq!(e.message.lines().count(), 1, "one line for the prompt");
    }

    #[test]
    fn an_empty_literal_asks_for_the_cells_with_nothing_in_them() {
        let Command::Filter(f) = ok(r#"filter name = """#) else {
            panic!("not a filter")
        };
        assert!(matches!(f.conditions[0].value, Value::Empty));
    }

    #[test]
    fn conditions_join_with_and() {
        let Command::Filter(f) = ok("filter count > 10 and name ~ ^a") else {
            panic!("not a filter")
        };
        assert_eq!(f.conditions.len(), 2);
        assert_eq!(f.conditions[1].column, 0);
    }

    #[test]
    fn a_malformed_condition_says_what_the_shape_should_be() {
        assert!(err("filter count").message.contains("column op value"));
        assert!(err("filter count >").message.contains("column op value"));
        assert!(
            err("filter count % 3")
                .message
                .contains("is not a comparison")
        );
        assert!(
            err("filter count > 1 and")
                .message
                .contains("needs another condition"),
            "a dangling `and` is caught"
        );
        assert!(
            err("filter count > 1 name = a")
                .message
                .contains("expected `and`"),
            "two conditions must be joined, not merely adjacent"
        );
    }

    // ── reset, quoting, verbs ────────────────────────────────────────────

    #[test]
    fn reset_takes_a_slot_or_clears_everything() {
        assert!(matches!(ok("reset"), Command::Reset(None)));
        assert!(matches!(
            ok("reset filter"),
            Command::Reset(Some(Slot::Filter))
        ));
        assert!(matches!(
            ok("reset hide"),
            Command::Reset(Some(Slot::Select))
        ));
        assert!(
            err("reset colour")
                .message
                .contains("try select, filter or sort")
        );
    }

    #[test]
    fn an_unclosed_quote_is_reported_rather_than_guessed_at() {
        let e = err(r#"select "release"#);
        assert_eq!(e.message, "unclosed quote");
    }

    #[test]
    fn an_unknown_verb_is_named() {
        assert_eq!(err("frobnicate x").message, "not a command: :frobnicate");
    }

    // ── view state ───────────────────────────────────────────────────────

    #[test]
    fn a_command_replaces_its_slot_rather_than_composing() {
        let mut view = View::default();
        view.apply(ok("select name count"), 6).unwrap();
        assert_eq!(view.columns(6), [0, 1]);

        // Not "select ratio from the two already showing", which would be an
        // error — the second command simply says what to show now.
        view.apply(ok("select ratio"), 6).unwrap();
        assert_eq!(view.columns(6), [2]);
    }

    #[test]
    fn hide_narrows_what_is_on_show() {
        let mut view = View::default();
        view.apply(ok("hide ratio ok"), 6).unwrap();
        assert_eq!(view.columns(6), [0, 1, 4, 5]);

        view.apply(ok("hide name"), 6).unwrap();
        assert_eq!(view.columns(6), [1, 4, 5], "hiding again narrows further");

        view.apply(ok("select name"), 6).unwrap();
        assert_eq!(view.columns(6), [0], "but select replaces the lot");
    }

    #[test]
    fn hiding_everything_is_refused() {
        let mut view = View::default();
        let all = ok("hide name count ratio ok \"release date\" \"odd-\"");
        assert!(view.apply(all, 6).is_err());
        assert_eq!(view.columns(6), (0..6).collect::<Vec<_>>(), "unchanged");
    }

    #[test]
    fn a_filter_and_a_sort_are_refused_together() {
        let mut view = View::default();
        view.apply(ok("sort name"), 6).unwrap();
        let e = view.apply(ok("filter count > 1"), 6).unwrap_err();
        assert!(e.contains("sorted view"), "{e}");
        assert!(view.filter.is_none(), "and the sort is left alone");

        view.apply(ok("sort"), 6).unwrap();
        view.apply(ok("filter count > 1"), 6).unwrap();
        let e = view.apply(ok("sort name"), 6).unwrap_err();
        assert!(e.contains("filtered view"), "{e}");

        // Clearing either one is always allowed.
        view.apply(ok("sort"), 6).unwrap();
        view.apply(ok("filter"), 6).unwrap();
        assert!(view.is_empty());
    }

    #[test]
    fn reset_clears_one_slot_or_all_of_them() {
        let mut view = View::default();
        view.apply(ok("select name"), 6).unwrap();
        view.apply(ok("filter count > 1"), 6).unwrap();

        view.apply(ok("reset filter"), 6).unwrap();
        assert!(view.filter.is_none());
        assert!(view.select.is_some(), "the other slots are untouched");

        view.apply(ok("sort name-"), 6).unwrap();
        view.apply(ok("reset sort"), 6).unwrap();
        assert!(view.sort.is_empty());
        assert!(view.select.is_some());

        view.apply(ok("reset"), 6).unwrap();
        assert!(view.is_empty());
    }

    #[test]
    fn the_view_describes_itself_for_the_status_bar() {
        let schema = schema();
        let mut view = View::default();
        assert_eq!(view.describe(&schema), None);

        view.apply(ok("select name count"), 6).unwrap();
        view.apply(ok("filter count > 10 and name ~ ^a"), 6)
            .unwrap();
        assert_eq!(
            view.describe(&schema).unwrap(),
            "select 2/6  count > 10 and name ~ ^a"
        );

        view.apply(ok("filter"), 6).unwrap();
        view.apply(ok("sort count-"), 6).unwrap();
        assert_eq!(view.describe(&schema).unwrap(), "select 2/6  sort count-");
    }
}
