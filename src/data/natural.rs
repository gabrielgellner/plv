//! Natural order: a run of digits compared by what it counts, not character
//! by character.
//!
//! A text column holding `9%`, `23%` and `100%` sorts as `100%`, `23%`, `9%`
//! under byte order, because `'1'` is below `'2'` is below `'9'` and the
//! comparison never gets as far as noticing that one of the three is a
//! hundred. The same goes for `file10` against `file2`, `v1.10` against
//! `v1.9`, and every identifier anyone ever wrote a number into.
//!
//! **This is not a claim about the data.** plv will not read `83%` as a
//! number — the column stays text, `:filter share > 50` still refuses it, and
//! the cell is drawn as the bytes it holds. All that changes is the order the
//! rows come out in, which is a fact about the sort and not about the file.
//! That is the line: [`super::store`] casts nothing and infers nothing here.
//!
//! It is on by default rather than behind a flag, because of *when* it
//! differs from byte order: only where two digit runs have different lengths.
//! Runs of equal length — a hash, a zero-padded id, an ISO date, a uuid —
//! compare identically either way, so the rows it reorders are exactly the
//! rows byte order was getting wrong. A key nobody has to ask for is worth
//! more than one nobody remembers.

/// The longest digit run whose length the key records exactly. Past this two
/// runs fall back to comparing their digits, which is a long way past the
/// point where a number that long was still being used as a number.
const MAX_RUN: usize = 999;

/// How many characters the length marker takes. Wide enough for [`MAX_RUN`].
const MARKER: usize = 3;

/// A sort key whose byte order is `value`'s natural order.
///
/// Every digit run is emitted behind a marker counting its *significant*
/// digits, so a longer number sorts after a shorter one whatever its digits
/// are, and runs of the same length then compare digit by digit — which for
/// equal-length numbers is already the right answer. Everything that is not a
/// digit is copied through untouched, so text with no numbers in it keys to
/// itself and sorts exactly as it does today.
///
/// The digits are kept as written rather than stripped of leading zeros, so
/// `03` and `3` are ordered rather than tied — the same value, but not the
/// same cell, and a sort that shuffles them from run to run is one that looks
/// broken.
///
/// The marker's own characters are digits, which cannot be mistaken for the
/// value's: every digit in the input belongs to a run and is emitted behind a
/// marker of its own, so nothing else in the key can be read as one.
pub fn key(value: &str) -> String {
    let bytes = value.as_bytes();
    // The key is the value plus a marker per digit run. One run is the common
    // case and no allocation is the point of guessing at all.
    let mut out = String::with_capacity(value.len() + MARKER);
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        if bytes[at].is_ascii_digit() {
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            let run = &value[start..at];
            let zeros = run.bytes().take_while(|b| *b == b'0').count();
            let significant = (run.len() - zeros).min(MAX_RUN);
            out.push_str(&format!("{significant:0MARKER$}"));
            out.push_str(run);
        } else {
            // A digit is one byte and never part of a multi-byte character,
            // so every boundary this walk lands on is a character boundary.
            while at < bytes.len() && !bytes[at].is_ascii_digit() {
                at += 1;
            }
            out.push_str(&value[start..at]);
        }
    }
    out
}

/// Whether `values` would come out in a different order than byte order puts
/// them — which is the only reason to pay for a key column at all.
///
/// The keys of two values diverge where their text does, and text is copied
/// through untouched; the only thing that can reorder them is a *marker*,
/// which differs only where two runs reached by the same prefix count
/// different numbers of digits. So the question is per position: whether the
/// column's first digit run is ever a different length than another row's
/// first, its second than another's second, and so on.
///
/// Comparing every run against every other instead would answer yes to a
/// column of dates, whose runs are 4, 2 and 2 within a single value and
/// perfectly ordered by byte comparison.
///
/// One pass, allocating nothing, and short-circuiting the moment it knows.
pub fn worth_keying<'a>(values: impl IntoIterator<Item = &'a str>) -> bool {
    let mut seen: Vec<usize> = Vec::new();
    for value in values {
        for (nth, run) in runs(value).enumerate() {
            match seen.get(nth) {
                None => seen.push(run),
                Some(&first) if first != run => return true,
                Some(_) => {}
            }
        }
    }
    false
}

/// How many characters each of `value`'s digit runs is, in order.
///
/// Written characters and not significant digits, even though the marker
/// counts the latter, because *equal written length is what makes the two
/// orders agree*: among runs of the same width, more significant digits
/// means a larger number, so the markers rank them the same way comparing
/// the digits would, and equal markers leave equal-width digits to compare
/// numerically by themselves. Counting significant digits here would call a
/// column of `01`, `09`, `12` uneven and key every zero-padded date and
/// timestamp in the file for an order it already had.
fn runs(value: &str) -> impl Iterator<Item = usize> + '_ {
    let bytes = value.as_bytes();
    let mut at = 0;
    std::iter::from_fn(move || {
        while at < bytes.len() && !bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at == bytes.len() {
            return None;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        Some((at - start).min(MAX_RUN))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the keys say the order is, which is the only thing this module
    /// claims. Sorting the keys and reading the values back is the same shape
    /// the store uses them in.
    fn ordered<'a>(values: &[&'a str]) -> Vec<&'a str> {
        let mut keyed: Vec<(String, &str)> = values.iter().map(|v| (key(v), *v)).collect();
        keyed.sort();
        keyed.into_iter().map(|(_, v)| v).collect()
    }

    #[test]
    fn percentages_order_by_what_they_count() {
        assert_eq!(
            ordered(&["100%", "9%", "0%", "23%", "50%"]),
            ["0%", "9%", "23%", "50%", "100%"]
        );
    }

    #[test]
    fn a_number_inside_a_name_counts_as_one() {
        assert_eq!(
            ordered(&["file10", "file2", "file1"]),
            ["file1", "file2", "file10"]
        );
    }

    #[test]
    fn every_run_is_compared_in_turn() {
        assert_eq!(
            ordered(&["v1.10", "v1.9", "v1.2"]),
            ["v1.2", "v1.9", "v1.10"]
        );
    }

    /// The property the default rests on: where byte order was already right,
    /// nothing moves. Equal-length runs are the whole of that case.
    #[test]
    fn equal_length_runs_keep_the_order_they_had() {
        let dates = ["2024-01-09", "2023-12-31", "2024-01-10"];
        let mut byte_order = dates;
        byte_order.sort();
        assert_eq!(ordered(&dates), byte_order);
    }

    #[test]
    fn text_with_no_digits_keys_to_itself() {
        assert_eq!(key("close"), "close");
        assert_eq!(key(""), "");
    }

    /// A tie here would let two rows swap places between one sort and the
    /// next, which reads as a bug whichever of them lands on top.
    #[test]
    fn leading_zeros_are_ordered_rather_than_tied() {
        assert_ne!(key("03"), key("3"));
        assert_eq!(ordered(&["3", "03"]), ["03", "3"]);
    }

    #[test]
    fn a_run_of_zeros_is_a_number_like_any_other() {
        assert_eq!(ordered(&["000", "0", "1"]), ["0", "000", "1"]);
    }

    /// The marker is digits, and so is the value it precedes. They cannot be
    /// confused, because a digit in the value is never emitted without one.
    #[test]
    fn a_marker_is_not_read_as_part_of_the_value() {
        assert_eq!(ordered(&["a1b", "a11"]), ["a1b", "a11"]);
        assert_eq!(ordered(&["1a", "11a", "2a"]), ["1a", "2a", "11a"]);
    }

    #[test]
    fn multi_byte_characters_survive() {
        assert_eq!(key("café2"), "café2".replace('2', "0012"));
        assert_eq!(ordered(&["é10", "é9"]), ["é9", "é10"]);
    }

    #[test]
    fn a_column_of_equal_length_runs_is_not_worth_keying() {
        assert!(!worth_keying(["2024-01-09", "2023-12-31"]));
        assert!(!worth_keying(["close", "open"]));
        assert!(!worth_keying(Vec::<&str>::new()));
        // Zero-padded: unequal as numbers, equal as written, and already in
        // the right order without a key.
        assert!(!worth_keying(["01", "09", "12"]));
    }

    /// The property `worth_keying` is allowed to be cheap because of: where
    /// it says no, the key would have produced the order the column already
    /// had. A false yes only costs a column; a false no would be a wrong
    /// answer on screen.
    #[test]
    fn saying_no_means_the_key_would_have_changed_nothing() {
        for column in [
            ["2024-01-09", "2023-12-31", "2024-12-01"],
            ["01", "09", "12"],
            ["b", "a", "c"],
            ["x1y", "x9y", "x5y"],
        ] {
            if worth_keying(column) {
                continue;
            }
            let mut natural = column;
            natural.sort_by_key(|v| key(v));
            let mut bytes = column;
            bytes.sort();
            assert_eq!(natural, bytes, "{column:?}");
        }
    }

    /// A date holds runs of 4, 2 and 2, and is perfectly ordered by byte
    /// comparison. Reading those three as "the lengths differ" would key
    /// every date column in every file for nothing.
    #[test]
    fn runs_are_compared_by_position_and_not_against_each_other() {
        assert!(!worth_keying(["2024-01-09"]));
        assert!(worth_keying(["2024-01-09", "999-01-09"]));
    }

    #[test]
    fn a_column_whose_runs_differ_in_length_is() {
        assert!(worth_keying(["9%", "100%"]));
        assert!(worth_keying(["file2", "file10"]));
    }
}
