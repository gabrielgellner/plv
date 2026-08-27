use clap::Parser;
use std::path::PathBuf;

use plv::app::App;

#[derive(Parser)]
#[command(
    name = "plv",
    version,
    about = "Polars CSV/Parquet/DuckLake viewer"
)]
struct Args {
    /// File to open: .csv, .parquet, a .ducklake catalog, or a bundle directory
    file: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut terminal = ratatui::init();
    let mut app = App::new(args.file);
    let result = app.run(&mut terminal);
    ratatui::restore();
    result?;
    Ok(())
}
