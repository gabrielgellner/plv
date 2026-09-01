use anyhow::Result;
use polars::prelude::*;
use std::io::Read;
use std::path::Path;

pub enum FileFormat {
    Csv,
    /// Extension names tabs: `.tsv`, `.tab`.
    Tsv,
    /// Extension names nothing: `.txt`. The delimiter is sniffed from the file.
    Text,
    Parquet,
    Unknown,
}

pub fn detect_format(path: &Path) -> FileFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("csv") => FileFormat::Csv,
        Some("tsv" | "tab") => FileFormat::Tsv,
        Some("txt") => FileFormat::Text,
        Some("parquet") => FileFormat::Parquet,
        _ => FileFormat::Unknown,
    }
}

pub fn load(path: &Path) -> Result<LazyFrame> {
    let pl_path = PlRefPath::try_from_path(path)?;
    match detect_format(path) {
        FileFormat::Csv => Ok(delimited(pl_path, b',')?),
        FileFormat::Tsv => Ok(delimited(pl_path, b'\t')?),
        FileFormat::Text => Ok(delimited(pl_path, sniff_delimiter(&read_sample(path)?))?),
        FileFormat::Parquet => Ok(LazyFrame::scan_parquet(pl_path, Default::default())?),
        FileFormat::Unknown => {
            anyhow::bail!("unsupported file format (use .csv, .tsv, .tab, .txt or .parquet)")
        }
    }
}

fn delimited(path: PlRefPath, separator: u8) -> PolarsResult<LazyFrame> {
    LazyCsvReader::new(path).with_separator(separator).finish()
}

/// How much of a file to look at when sniffing its delimiter.
const SAMPLE_BYTES: u64 = 64 * 1024;

/// Candidates, in the order ties are broken.
const DELIMITERS: [u8; 4] = *b"\t,;|";

/// Lines to compare before deciding. The header alone can be misleading.
const SAMPLE_LINES: usize = 5;

fn read_sample(path: &Path) -> Result<Vec<u8>> {
    let mut sample = Vec::new();
    std::fs::File::open(path)?
        .take(SAMPLE_BYTES)
        .read_to_end(&mut sample)?;
    Ok(sample)
}

/// Pick the field separator for a file whose extension doesn't name one.
///
/// Counts each candidate outside double-quoted spans on the first few lines and
/// keeps the one that occurs at least once and the *same* number of times on
/// every line — that consistency is what separates a real delimiter from a
/// character the text merely happens to contain. A sample that settles nothing
/// (one line, no repeats, a stray delimiter inside a quoted field spanning a
/// newline) falls back to a tab, which is what `.txt` usually means.
fn sniff_delimiter(sample: &[u8]) -> u8 {
    let lines: Vec<&[u8]> = sample
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .take(SAMPLE_LINES)
        .collect();

    DELIMITERS
        .into_iter()
        .find(|&delim| {
            let mut counts = lines.iter().map(|line| count_unquoted(line, delim));
            match counts.next() {
                Some(first) => first > 0 && counts.all(|n| n == first),
                None => false,
            }
        })
        .unwrap_or(b'\t')
}

/// Occurrences of `delim` in `line` that fall outside a quoted field. A doubled
/// `""` inside a quoted field toggles twice, which lands back where it started.
fn count_unquoted(line: &[u8], delim: u8) -> usize {
    let mut quoted = false;
    line.iter()
        .filter(|&&b| {
            if b == b'"' {
                quoted = !quoted;
            }
            !quoted && b == delim
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_separated_extensions_split_on_tabs() {
        for ext in ["tsv", "tab", "txt"] {
            let df = load(&write_temp(ext, "name\tcount\na\t1\nb\t2\n"))
                .unwrap()
                .collect()
                .unwrap();
            assert_eq!(df.shape(), (2, 2), "{ext}");
            assert_eq!(df.get_column_names(), ["name", "count"], "{ext}");
        }
    }

    #[test]
    fn comma_separated_txt_is_sniffed() {
        let df = load(&write_temp("txt", "name,count\na,1\nb,2\n"))
            .unwrap()
            .collect()
            .unwrap();
        assert_eq!(df.shape(), (2, 2));
        assert_eq!(df.get_column_names(), ["name", "count"]);
    }

    #[test]
    fn sniffs_each_candidate() {
        assert_eq!(sniff_delimiter(b"a\tb\n1\t2\n"), b'\t');
        assert_eq!(sniff_delimiter(b"a,b\n1,2\n"), b',');
        assert_eq!(sniff_delimiter(b"a;b\n1;2\n"), b';');
        assert_eq!(sniff_delimiter(b"a|b\n1|2\n"), b'|');
    }

    #[test]
    fn ignores_delimiters_inside_quotes() {
        // Every line has one real semicolon; the commas are all quoted away.
        let sample = b"name;note\n\"a,b\";\"x,y,z\"\n\"c,d\";\"p,q,r\"\n";
        assert_eq!(sniff_delimiter(sample), b';');
    }

    #[test]
    fn prose_and_empty_samples_fall_back_to_tab() {
        // Commas appear, but not the same number of times on every line.
        assert_eq!(sniff_delimiter(b"one, two, three\nfour five\n"), b'\t');
        assert_eq!(sniff_delimiter(b""), b'\t');
    }

    #[test]
    fn windows_line_endings_do_not_break_counting() {
        assert_eq!(sniff_delimiter(b"a,b\r\n1,2\r\n"), b',');
    }

    fn write_temp(ext: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("plv-loader-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("sample-{ext}-{:x}.{ext}", contents.len()));
        std::fs::write(&path, contents).unwrap();
        path
    }
}
