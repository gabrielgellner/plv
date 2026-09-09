use clap::Parser;
use std::path::PathBuf;

use plv::app::App;

#[derive(Parser)]
#[command(
    name = "plv",
    version,
    about = "Polars CSV/TSV/JSONL/Parquet/DuckLake viewer"
)]
struct Args {
    /// File to open: .csv, .tsv/.tab/.txt, .jsonl/.ndjson, .parquet, a .ducklake catalog, or a bundle directory
    file: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut terminal = ratatui::init();
    // Bracketed paste is how plv is handed what the terminal's own paste key
    // pasted — the read half of the clipboard, which OSC 52 does not have.
    // A terminal that does not answer simply never sends the event.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let mut app = App::new(args.file);
    let result = app.run(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result?;
    Ok(())
}
