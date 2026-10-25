// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation, a menu bar app; `export`, `import`, `reset` on the command line.

mod audio;
mod cli;
mod ctl;
mod engine;
mod history;
// macOS first, Windows and Linux later (docs/plans/20261023-port.md).
#[cfg(target_os = "macos")]
mod gui;
mod session;
mod settings;
#[cfg(test)]
mod testing;
mod transfer;
#[cfg(any(test, target_os = "macos"))]
mod ui;
mod window;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nimbo::{AppInfo, Presence};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

const INFO: AppInfo =
  AppInfo { id: "me.vladkens.wavo", name: "Wavo", presence: Presence::Accessory };

#[derive(Parser)]
#[command(version, about = "Local dictation", subcommand_required = !cfg!(target_os = "macos"))]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  /// Write the settings, the history and its recordings into one archive (a tar)
  Export { file: PathBuf },
  /// Add the entries of an archive the history does not have; Wavo must not be running
  Import {
    source: PathBuf,
    /// Also replace the settings with the archive's
    #[arg(long)]
    settings: bool,
    /// Say what would be imported, write nothing
    #[arg(long)]
    dry_run: bool,
  },
  /// Quit Wavo, turn launch at login off, reset its permissions and delete its data and cache
  Reset {
    /// Say what it would do, change nothing
    #[arg(long)]
    dry_run: bool,
    /// Do not ask first
    #[arg(short, long)]
    yes: bool,
  },
  /// Quit the running app
  #[command(hide = true)]
  Quit,
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  init_tracing();
  match cli.command {
    #[cfg(target_os = "macos")]
    None => gui::run(),
    #[cfg(not(target_os = "macos"))]
    None => unreachable!("clap asks for a command"),
    Some(Command::Export { file }) => cli::export(&file),
    Some(Command::Import { source, settings, dry_run }) => cli::import(&source, settings, dry_run),
    Some(Command::Reset { dry_run, yes }) => cli::reset(dry_run, yes),
    Some(Command::Quit) => cli::quit(),
  }
}

/// Settings and history: `$WAVO_HOME`, else the platform's data dir. Models are in the Hugging
/// Face cache.
fn home() -> PathBuf {
  match std::env::var_os("WAVO_HOME") {
    Some(dir) if !dir.is_empty() => PathBuf::from(dir),
    _ => nimbo::paths::data_dir(INFO.id),
  }
}

/// Logs go to stderr, filtered by `WAVO_LOG` (`warn,wavo_app=debug`).
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
