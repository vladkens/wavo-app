// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation. For now its command-line tools.

mod audio;
mod cli;
mod models;
#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(version, about = "Local dictation")]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// List audio input devices
  Devices,
  /// Record from the microphone (or the WAV file `WAVO_FAKE_MIC` names) into a 16 kHz mono WAV
  Record {
    #[arg(long, value_parser = cli::seconds)]
    seconds: Duration,
    /// Input device ID or name (see `wavo-app devices`); a missing one falls back to the default
    #[arg(long)]
    device: Option<String>,
    out: PathBuf,
  },
  /// List the speech models in the Hugging Face cache
  Models,
  /// Download one of wavo's models (e.g. whisper-base) into the Hugging Face cache
  Pull { name: String },
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  init_tracing();
  match cli.command {
    Command::Devices => cli::devices(),
    Command::Record { seconds, device, out } => cli::record(seconds, device.as_deref(), &out),
    Command::Models => cli::models(),
    Command::Pull { name } => cli::pull(&name),
  }
}

/// Logs go to stderr, filtered by `WAVO_LOG` (`warn,wavo_app=debug` syntax). The default keeps
/// other crates at warnings.
fn init_tracing() {
  let filter = std::env::var("WAVO_LOG").ok().and_then(|f| f.parse::<Targets>().ok());
  let filter = filter.unwrap_or_else(|| {
    Targets::new().with_default(LevelFilter::WARN).with_target("wavo_app", LevelFilter::INFO)
  });
  tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
    .with(filter)
    .init();
}
