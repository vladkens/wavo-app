// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation. The menu bar app on macOS (`gui`, the default), the dictation
//! daemon and the command-line tools.

mod audio;
mod cli;
mod ctl;
mod engine;
mod history;
// The menu bar app: macOS first, Windows and Linux later (docs/plans/20261023-port.md).
#[cfg(target_os = "macos")]
mod gui;
mod language;
mod reset;
mod session;
mod settings;
#[cfg(test)]
mod testing;
mod transfer;
// The macOS shell's; elsewhere built for its tests.
#[cfg(any(test, target_os = "macos"))]
mod ui;
mod window;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nimbo::{AppInfo, Presence};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

use crate::ctl::Request;

/// The app as the system knows it: its menu bar presence, its login item.
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
  /// Run the menu bar app (the default without a command)
  #[cfg(target_os = "macos")]
  Gui,
  /// Transcribe a WAV file without capture or paste
  Transcribe {
    wav: PathBuf,
    /// Model: one of wavo's (`wavo list`: gigaam-v3, whisper-turbo…) in the Hugging Face cache, or
    /// a path to its GGUF file; by default the settings', else the first of wavo's models
    /// downloaded
    #[arg(long)]
    model: Option<String>,
    /// Language code (`en`, `ru`…), or `auto` for the model to detect it; by default the
    /// settings'
    #[arg(long)]
    language: Option<String>,
  },
  /// Render the settings window headless with nimbo's harness flags (`--shot <png>`,
  /// `--appearance light|dark`, `--size <w>x<h>`, `--scale <n>`); `wavo-app ctl open-settings`
  /// opens the running app's
  Settings {
    /// The page it shows
    #[arg(long, value_enum, default_value = "general")]
    page: window::Page,
    /// nimbo's harness flags
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    harness: Vec<String>,
  },
  /// Run dictation without a GUI, controlled by `wavo-app ctl`; texts go to stdout
  Daemon,
  /// Control the running app
  Ctl {
    #[command(subcommand)]
    request: Request,
  },
  /// Write the settings, the history and its recordings into one archive (a tar), to import on
  /// another computer
  Export { file: PathBuf },
  /// Add to the history what it does not have of an archive of `wavo-app export`, or of Handy's
  /// history on this computer (`handy`); Wavo must not be running
  Import {
    /// The archive, or `handy` (its data directory, or `HANDY_DIR`)
    source: PathBuf,
    /// Also replace the settings with the archive's
    #[arg(long)]
    settings: bool,
    /// Say what would be imported, and write nothing
    #[arg(long)]
    dry_run: bool,
  },
  /// Quit Wavo, turn launch at login off, reset its permissions (macOS) and delete its data and
  /// caches, for a clean install; the Hugging Face cache stays
  Reset {
    /// Also delete the app this runs from (macOS)
    #[arg(long)]
    app: bool,
    /// Print what it would do, change nothing
    #[arg(long)]
    dry_run: bool,
    /// Do not ask first
    #[arg(short)]
    yes: bool,
  },
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
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  init_tracing();
  let Some(command) = cli.command else {
    #[cfg(target_os = "macos")]
    return gui::run();
    #[cfg(not(target_os = "macos"))]
    unreachable!("clap asks for a command");
  };

  match command {
    #[cfg(target_os = "macos")]
    Command::Gui => gui::run(),
    Command::Transcribe { wav, model, language } => {
      cli::transcribe(&wav, model.as_deref(), language.as_deref())
    }
    Command::Settings { page, harness } => cli::settings(page, &harness),
    Command::Daemon => cli::daemon(),
    Command::Ctl { request } => cli::ctl(request),
    Command::Export { file } => cli::export(&file),
    Command::Import { source, settings, dry_run } => cli::import(&source, settings, dry_run),
    Command::Reset { app, dry_run, yes } => reset::run(app, dry_run, yes),
    Command::Devices => cli::devices(),
    Command::Record { seconds, device, out } => cli::record(seconds, device.as_deref(), &out),
  }
}

/// The data home, where settings and history live: `$WAVO_HOME`, else the platform's per-user
/// data directory. Models are in the Hugging Face cache.
fn home() -> PathBuf {
  match std::env::var_os("WAVO_HOME") {
    Some(dir) if !dir.is_empty() => PathBuf::from(dir),
    _ => nimbo::paths::data_dir("me.vladkens.wavo"),
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
