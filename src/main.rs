// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation. For now headless: the dictation daemon and its command-line tools.

mod audio;
mod cli;
mod ctl;
mod engine;
mod history;
mod session;
mod settings;
#[cfg(test)]
mod testing;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

use crate::ctl::Request;

#[derive(Parser)]
#[command(version, about = "Local dictation")]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
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
  /// Run dictation without a GUI, controlled by `wavo-app ctl`; texts go to stdout
  Daemon,
  /// Control the running app
  Ctl {
    #[command(subcommand)]
    action: CtlAction,
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

#[derive(Subcommand)]
enum CtlAction {
  /// Start or stop dictation
  Toggle,
  /// Start dictation
  Start,
  /// Stop dictation and transcribe
  Stop,
  /// Cancel recording or transcription
  Cancel,
  /// Print the app's state as JSON
  Status,
  /// Transcribe a history entry's recording again
  Retranscribe { id: i64 },
  /// Delete a history entry and its recording
  Delete { id: i64 },
  /// Free the model's memory now; the next dictation loads it again
  Unload,
  /// Quit the running app
  Quit,
}

impl From<CtlAction> for Request {
  fn from(action: CtlAction) -> Self {
    match action {
      CtlAction::Toggle => Request::Toggle,
      CtlAction::Start => Request::Start,
      CtlAction::Stop => Request::Stop,
      CtlAction::Cancel => Request::Cancel,
      CtlAction::Status => Request::Status,
      CtlAction::Retranscribe { id } => Request::Retranscribe { id },
      CtlAction::Delete { id } => Request::Delete { id },
      CtlAction::Unload => Request::Unload,
      CtlAction::Quit => Request::Quit,
    }
  }
}

fn main() -> Result<()> {
  let cli = Cli::parse();
  init_tracing();
  match cli.command {
    Command::Transcribe { wav, model, language } => {
      cli::transcribe(&wav, model.as_deref(), language.as_deref())
    }
    Command::Daemon => cli::daemon(),
    Command::Ctl { action } => cli::ctl(action.into()),
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
