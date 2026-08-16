//! `synora` — unified CLI (spec §45). M0: `check` / `config validate`.

use clap::{Parser, Subcommand};
use config::{CliOverrides, ConfigLoader};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "synora", version, about = "Synora — mirror synchronization engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate configuration; errors report file:line (spec §44)
    Check {
        /// Main config file (default: synora.toml or config/synora.toml)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Configuration subcommands
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Same as `check`
    Validate {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let path = match &cli.command {
        Command::Check { config } => config.clone(),
        Command::Config {
            cmd: ConfigCmd::Validate { config },
        } => config.clone(),
    };
    let path = find_config(path)?;
    let cfg = ConfigLoader::load(&path, &CliOverrides::default()).map_err(|e| e.to_string())?;
    println!("config OK: {} job(s)", cfg.jobs.len());
    for j in &cfg.jobs {
        let state = if j.enabled { "enabled " } else { "disabled" };
        let provider = match &j.provider {
            domain::ProviderConfig::Rsync { .. } => "rsync",
            domain::ProviderConfig::Script { .. } => "script",
            domain::ProviderConfig::Docker { .. } => "docker",
        };
        println!(
            "  {:<20} {} {:<24} {:>7} {}",
            j.name,
            j.schedule.describe(),
            provider,
            state,
            j.storage.display()
        );
    }
    Ok(())
}

fn find_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("config file not found: {}", p.display()));
    }
    for candidate in ["synora.toml", "config/synora.toml"] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("no config file found (looked for synora.toml, config/synora.toml; use -c PATH)".into())
}
