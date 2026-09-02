//! Completion for the `:` line.
//!
//! Deliberately **not** fuzzy. This is a small closed vocabulary — a handful of
//! verbs and the file's own column names — that the user is trying to type
//! exactly. A prefix match is predictable; a fuzzy one is a guess, and the one
//! thing completion must never do is fight what someone is halfway through
//! typing. Fuzzy matching belongs on `/`, over the data.
//!
//! What can be completed depends on where in the line the cursor is: verbs at
//! the front, then column names, and inside a `:filter` the positions cycle
//! through column, operator, value, `and`.

use polars::prelude::Schema;

/// Verbs that shape the view. A test keeps this in step with the parser.
pub const VIEW_VERBS: &[&str] = &["select", "hide", "filter", "sort", "reset"];
/// Verbs that act on the file. Owned by the app layer, not the view language.
pub const FILE_VERBS: &[&str] = &["w", "wq", "q", "x"];

const OPERATORS: &[&str] = &["=", "!=", "<", "<=", ">", ">=", "~", "!~"];
const SLOTS: &[&str] = &["select", "filter", "sort"];

/// What a completion request found.
#[derive(Debug, PartialEq)]
pub struct Completion {
    /// Everything before the word being completed, trailing space included.
    pub head: String,
    /// The word as typed, so a caller can tell whether extending it is safe.
    pub word: String,
    /// The candidates, already quoted where they need to be.
    pub options: Vec<String>,
}

impl Completion {
    /// The line with the longest unambiguous extension applied.
    ///
    /// A single candidate replaces the word outright and gains a trailing
    /// space, because the next argument is what you want to type next. With
    /// several, the line only grows — never shrinks — so completion cannot eat
    /// characters that were typed on purpose.
    pub fn extended(&self) -> String {
        if let [only] = self.options.as_slice() {
            return format!("{}{only} ", self.head);
        }
        let shared = common_prefix(&self.options);
        if shared.len() > self.word.len() && shared.starts_with(&self.word) {
            format!("{}{shared}", self.head)
        } else {
            format!("{}{}", self.head, self.word)
        }
    }

    /// The line with one named candidate in place of the word.
    pub fn with(&self, index: usize) -> String {
        match self.options.get(index) {
            Some(option) => format!("{}{option} ", self.head),
            None => format!("{}{}", self.head, self.word),
        }
    }
}

/// Complete `line` — the command buffer, without its leading `:`.
///
/// `None` when nothing matches, so the caller leaves what was typed alone
/// rather than deleting it.
pub fn complete(line: &str, schema: &Schema) -> Option<Completion> {
    let (head, word) = split_last_word(line);
    let stem = word.trim_start_matches('"');

    // Matched against the bare name and offered in the form that has to be
    // typed: a column called `release date` is matched by `rel` but written
    // with quotes around it.
    let lower = stem.to_lowercase();
    let options: Vec<String> = candidates_for(head, schema)
        .into_iter()
        .filter(|(bare, _)| bare.to_lowercase().starts_with(&lower))
        .map(|(_, written)| written)
        .collect();

    (!options.is_empty()).then(|| Completion {
        head: head.to_string(),
        word: word.to_string(),
        options,
    })
}

/// What belongs where the cursor is, decided by the words already typed.
fn candidates_for(head: &str, schema: &Schema) -> Vec<(String, String)> {
    let words: Vec<&str> = head.split_whitespace().collect();
    let columns = || -> Vec<(String, String)> {
        schema
            .iter_names()
            .map(|n| (n.to_string(), quoted(n.as_str())))
            .collect()
    };
    let plain = |list: &[&str]| -> Vec<(String, String)> {
        list.iter()
            .map(|s| (s.to_string(), s.to_string()))
            .collect()
    };

    let Some(verb) = words.first() else {
        // Still on the first word.
        return plain(VIEW_VERBS)
            .into_iter()
            .chain(plain(FILE_VERBS))
            .collect();
    };

    match *verb {
        "select" | "hide" | "sort" => columns(),
        "reset" => plain(SLOTS),
        "filter" => {
            // Inside a filter the positions repeat: column, operator, value,
            // `and`. A value is the file's own data, which is not a vocabulary
            // to complete against.
            match (words.len() - 1) % 4 {
                0 => columns(),
                1 => plain(OPERATORS),
                2 => Vec::new(),
                _ => plain(&["and"]),
            }
        }
        _ => Vec::new(),
    }
}

/// A column name as it must be written to be read back as itself.
fn quoted(name: &str) -> String {
    let needs = name.is_empty()
        || name == "*"
        || name.ends_with('-')
        || name.chars().any(|c| c.is_whitespace() || c == '"');
    if needs {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

/// Split off the word the cursor sits at the end of, keeping the rest verbatim
/// so it can be put back unchanged.
fn split_last_word(line: &str) -> (&str, &str) {
    match line.rfind(char::is_whitespace) {
        Some(at) => line.split_at(at + 1),
        None => ("", line),
    }
}

fn common_prefix(options: &[String]) -> String {
    let Some(first) = options.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &options[1..] {
        end = end.min(
            first
                .char_indices()
                .zip(other.char_indices())
                .take_while(|((_, a), (_, b))| a == b)
                .map(|((i, a), _)| i + a.len_utf8())
                .last()
                .unwrap_or(0),
        );
    }
    first[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view;
    use polars::prelude::{DataType, PlSmallStr};

    fn schema() -> Schema {
        Schema::from_iter([
            (PlSmallStr::from_static("name"), DataType::String),
            (PlSmallStr::from_static("count"), DataType::Int64),
            (PlSmallStr::from_static("category"), DataType::String),
            (PlSmallStr::from_static("release date"), DataType::String),
        ])
    }

    fn options(line: &str) -> Vec<String> {
        complete(line, &schema())
            .map(|c| c.options)
            .unwrap_or_default()
    }

    fn extended(line: &str) -> String {
        complete(line, &schema())
            .map(|c| c.extended())
            .unwrap_or_else(|| line.to_string())
    }

    /// The list of verbs offered has to be the list the parser accepts, or
    /// completion will happily type something that then fails.
    #[test]
    fn every_offered_view_verb_is_one_the_parser_knows() {
        let schema = schema();
        for verb in VIEW_VERBS {
            assert!(
                view::parse(verb, &schema).is_ok(),
                "completion offers :{verb}, which the parser rejects"
            );
        }
        assert!(view::parse("frobnicate", &schema).is_err());
    }

    #[test]
    fn the_first_word_completes_to_a_verb() {
        assert_eq!(extended("sel"), "select ");
        assert_eq!(options("s"), ["select", "sort"]);
        // Nothing typed yet offers everything there is.
        assert!(options("").len() > 5);
    }

    #[test]
    fn a_column_argument_completes_to_a_column() {
        assert_eq!(extended("select na"), "select name ");
        assert_eq!(options("select c"), ["count", "category"]);
        assert_eq!(extended("hide co"), "hide count ");
    }

    #[test]
    fn a_name_that_needs_quoting_arrives_quoted() {
        assert_eq!(extended("select rel"), "select \"release date\" ");
        // And it can be completed from inside the quote, too.
        assert_eq!(extended("select \"rel"), "select \"release date\" ");
    }

    #[test]
    fn several_candidates_extend_only_as_far_as_they_agree() {
        // `count` and `category` share `c`, and the word is already `c`.
        assert_eq!(extended("select c"), "select c", "nothing to add yet");
        assert_eq!(extended("select ca"), "select category ");
    }

    #[test]
    fn completion_never_shortens_what_was_typed() {
        // `rel` matches only a quoted candidate, whose common prefix does not
        // start with what was typed. The word must survive.
        let c = complete("select re", &schema()).unwrap();
        assert!(
            c.extended().ends_with("\"release date\" "),
            "{}",
            c.extended()
        );

        let c = Completion {
            head: "select ".to_string(),
            word: "xyz".to_string(),
            options: vec!["abc".to_string(), "abd".to_string()],
        };
        assert_eq!(c.extended(), "select xyz", "left exactly as typed");
    }

    #[test]
    fn a_filter_cycles_through_column_operator_value_and_and() {
        assert_eq!(options("filter co"), ["count"]);
        assert_eq!(options("filter count "), OPERATORS);
        assert!(options("filter count > ").is_empty(), "a value is data");
        assert_eq!(options("filter count > 10 "), ["and"]);
        assert_eq!(options("filter count > 10 and na"), ["name"]);
    }

    #[test]
    fn reset_offers_the_slots() {
        assert_eq!(options("reset "), SLOTS);
        assert_eq!(extended("reset fi"), "reset filter ");
    }

    #[test]
    fn an_unknown_verb_has_nothing_to_offer() {
        assert!(complete("w some", &schema()).is_none());
        assert!(complete("select zzz", &schema()).is_none());
    }

    #[test]
    fn stepping_through_candidates_replaces_the_word() {
        let c = complete("select c", &schema()).unwrap();
        assert_eq!(c.with(0), "select count ");
        assert_eq!(c.with(1), "select category ");
        assert_eq!(c.with(9), "select c", "out of range leaves it alone");
    }
}
