// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! wavo-app: local dictation, a menu bar or tray app; `uninstall` on the command line.

// No console window next to the app on Windows; a command attaches to the one it runs in.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio;
mod cli;
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

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nimbo::desktop::Launcher;
use nimbo::{AppInfo, Config, Png, Presence};
use tracing::warn;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// The build, as the app shows it: `v0.1.0-12-g3672a1b` (build.rs's `WAVO_VERSION`).
const VERSION: &str = concat!("v", env!("WAVO_VERSION"));

const INFO: AppInfo =
  AppInfo { id: "me.vladkens.wavo", name: "Wavo", presence: Presence::Accessory };

/// The app's icon, decoded by nimbo when first shown.
const ICON: &[u8] = include_bytes!("../assets/icon.png");

/// The app's log in its home, [home]: `~/Library/Application Support/me.vladkens.wavo/wavo.log`,
/// `%LOCALAPPDATA%\me.vladkens.wavo\wavo.log`, `~/.local/share/me.vladkens.wavo/wavo.log`.
const LOG_FILE: &str = "wavo.log";
/// A log past this at launch is started over.
const LOG_MAX: u64 = 5 << 20;

/// The launcher entry on Linux, as the AppImage's .desktop has it (Cargo.toml's
/// `[package.metadata.nimbo]`).
const LAUNCHER: Launcher = Launcher {
  comment: "Local dictation: hold a hotkey, speak, and the text lands where you were typing",
  categories: &["Utility", "Accessibility"],
  icon: Png::icon(ICON),
};

/// `cfg` with the app's icon in the title bar and taskbar; macOS shows the bundle's.
fn with_icon(cfg: Config) -> Config {
  #[cfg(not(target_os = "macos"))]
  let cfg = cfg.png_icon(Png::icon(ICON));

  cfg
}

#[derive(Parser)]
#[command(version = VERSION, about = "Local dictation")]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  #[command(about = cli::UNINSTALL)]
  Uninstall {
    /// Say what it would do, change nothing
    #[arg(long)]
    dry_run: bool,
    #[arg(short, long, help = cli::YES)]
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
  // Windows: only for arguments (a command, `--help`, a mistake), which print. Attached, the app
  // would print its logs after the prompt, and closing that console would end it without its quit
  // hook.
  if std::env::args_os().len() > 1 {
    nimbo::attach_console();
  }

  let cli = Cli::parse();
  // The app logs into a file too: launched from the desktop it has no stderr.
  init_tracing(cli.command.is_none().then(home).as_deref());
  match cli.command {
    None => app::run(),
    Some(Command::Uninstall { dry_run, yes }) => cli::uninstall(dry_run, yes),
    Some(Command::Toggle) => cli::send("toggle"),
    Some(Command::Quit) => cli::send("quit"),
  }
}

/// Settings, history and the log: `$WAVO_HOME`, else the platform's data dir. Models are in the
/// Hugging Face cache.
fn home() -> PathBuf {
  match std::env::var_os("WAVO_HOME") {
    Some(dir) if !dir.is_empty() => PathBuf::from(dir),
    _ => nimbo::paths::data_dir(INFO.id),
  }
}

/// Logs go to stderr and, given a home, to its [LOG_FILE], filtered by `WAVO_LOG`
/// (`warn,wavo_app=debug`).
fn init_tracing(home: Option<&Path>) {
  let filter = std::env::var("WAVO_LOG").ok().and_then(|f| f.parse::<Targets>().ok());
  let filter = filter.unwrap_or_else(|| {
    Targets::new().with_default(LevelFilter::WARN).with_target("wavo_app", LevelFilter::INFO)
  });

  let (file, error) = match home.map(open_log).transpose() {
    Ok(file) => (file, None),
    Err(e) => (None, Some(e)),
  };

  tracing_subscriber::registry()
    .with(fmt::layer().with_writer(io::stderr))
    .with(file.map(|f| fmt::layer().with_ansi(false).with_writer(Arc::new(f))))
    .with(filter)
    .init();

  if let Some(e) = error {
    warn!("the log file: {e}");
  }
}

/// [LOG_FILE] in `home`, appended to; started over when past [LOG_MAX].
fn open_log(home: &Path) -> io::Result<File> {
  fs::create_dir_all(home)?;
  let path = home.join(LOG_FILE);
  if fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX) {
    File::create(&path)?;
  }

  File::options().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use super::*;
  use crate::testing::TempDir;

  #[test]
  fn the_log_is_appended_to_until_too_big() {
    let dir = TempDir::new("log");
    let home = dir.path().join("home");
    let path = home.join(LOG_FILE);
    writeln!(open_log(&home).unwrap(), "one").unwrap();
    writeln!(open_log(&home).unwrap(), "two").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n", "the home made, then appended");

    File::options().write(true).open(&path).unwrap().set_len(LOG_MAX + 1).unwrap();
    writeln!(open_log(&home).unwrap(), "three").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "three\n", "started over");
  }
}
