//! Standalone `synora-tui` binary — thin wrapper over the `tui` library.
//! The same console is also reachable as `synora tui`.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "synora-tui", version, about = "Synora terminal console")]
struct Cli {
    /// Main config file (also the file proxy registration writes to)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Manager URL override (default: config api.listen)
    #[arg(long)]
    manager: Option<String>,
    /// API token override (default: first configured token)
    #[arg(long)]
    token: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = tui::run(cli.config, cli.manager, cli.token) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
