mod app;
mod data;
mod ui;

use clap::Parser;
use std::path::PathBuf;

use app::App;

#[derive(Parser)]
#[command(name = "plv", about = "Polars CSV/Parquet viewer")]
struct Args {
    /// File to open (CSV or Parquet)
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
