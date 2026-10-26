// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation, a menu bar or tray app; `uninstall` on the command line.

// No console window next to the app on Windows; a command attaches to the one it runs in.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio;
mod cli;
mod ctl;
mod engine;
mod history;
mod hud;
mod menu;
mod session;
mod settings;
#[cfg(test)]
mod testing;
mod transfer;
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
#[command(version, about = "Local dictation")]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  /// Remove Wavo and all it left: its data, caches, defaults, permissions, the models it downloaded
  /// and the app
  Uninstall {
    /// Say what it would do, change nothing
    #[arg(long)]
    dry_run: bool,
    /// Do not ask first
    #[arg(short, long)]
    yes: bool,
  },
  /// Start or stop a dictation in the running app: for a shortcut in the desktop's own settings
  /// where Wavo's hotkey can't be (GNOME before 48 on Wayland)
  Toggle,
  /// Quit the running app
  #[command(hide = true)]
  Quit,
}

fn main() -> Result<()> {
  #[cfg(windows)]
  attach_console();

  let cli = Cli::parse();
  init_tracing();
  match cli.command {
    None => app::run(),
    Some(Command::Uninstall { dry_run, yes }) => cli::uninstall(dry_run, yes),
    Some(Command::Toggle) => cli::send("toggle"),
    Some(Command::Quit) => cli::send("quit"),
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

/// The console of the command line that started the app, if one did: its output and `--help` go
/// there (the app itself has none, as a GUI program).
#[cfg(windows)]
fn attach_console() {
  use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

  // SAFETY: no preconditions; it fails harmlessly without a parent console.
  unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
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
