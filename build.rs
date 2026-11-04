// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The build script: the version the app shows, `WAVO_VERSION` ([version]), and the flavor,
//! `cfg(wavo_dev)` ([dev]).

use std::path::PathBuf;
use std::process::Command;

fn main() {
  println!("cargo:rustc-env=WAVO_VERSION={}", version());
  println!("cargo:rustc-check-cfg=cfg(wavo_dev)");
  if dev() {
    println!("cargo:rustc-cfg=wavo_dev");
  }
}

/// `WAVO_FLAVOR=dev` (`make run`'s bundle): Wavo Dev, an app of its own beside the release
/// (main.rs's `DEV`); unset, empty or `release`: Wavo.
fn dev() -> bool {
  println!("cargo:rerun-if-env-changed=WAVO_FLAVOR");
  match std::env::var("WAVO_FLAVOR").unwrap_or_default().as_str() {
    "" | "release" => false,
    "dev" => true,
    other => panic!("WAVO_FLAVOR={other}: release or dev"),
  }
}

/// The version the build shows, as CI's dev builds have it (vladkens/actions dev-version): Cargo's
/// when CI stamped it already (a pre-release part); else `git describe` against the last vX.Y.Z
/// tag, `0.9.0-12-g3672a1b` after one, `0.1.0-g3672a1b` before any, the plain version on its tag
/// and on its release commit (`release: vX.Y.Z`, which CI builds before the tag is made: a
/// release's files show the version they are published as); `-dirty` with uncommitted changes;
/// Cargo's without git (a crate build).
fn version() -> String {
  let pkg = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
  if pkg.contains('-') {
    return pkg;
  }

  let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else { return pkg };
  rerun_on_git();
  let subject = git(&["log", "-1", "--format=%s"]);
  let release = subject.is_some_and(|s| s.starts_with(&format!("release: v{pkg}")));
  let described = git(&["describe", "--tags", "--match", "v[0-9]*.[0-9]*.[0-9]*"]);
  let version = match described {
    _ if release => pkg,
    Some(d) => d.trim_start_matches('v').to_owned(),
    None => format!("{pkg}-g{sha}"),
  };

  let dirty =
    git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
  if dirty { format!("{version}-dirty") } else { version }
}

/// `git` in the package's folder: its trimmed output, `None` when it fails or there is no git.
fn git(args: &[&str]) -> Option<String> {
  let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
  Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The version again when HEAD moves, a tag comes or the tree changes: HEAD and the index (in a
/// worktree's own git dir), the ref HEAD names and the tags (in the shared one), and the sources.
fn rerun_on_git() {
  let dir = |arg| git(&["rev-parse", arg]).map(PathBuf::from);
  let (Some(own), Some(shared)) = (dir("--absolute-git-dir"), dir("--git-common-dir")) else {
    return;
  };

  let shared = std::env::current_dir().map(|cwd| cwd.join(&shared)).unwrap_or(shared);
  let head = git(&["rev-parse", "--symbolic-full-name", "HEAD"]).filter(|r| r.starts_with("refs/"));
  let mut paths = vec![own.join("HEAD"), own.join("index"), shared.join("packed-refs")];
  paths.push(shared.join("refs/tags"));
  paths.extend(head.map(|r| shared.join(r)));
  paths.extend(["src", "Cargo.toml", "Cargo.lock"].map(PathBuf::from));
  // A path that isn't there would run this every build.
  for path in paths.iter().filter(|p| p.exists()) {
    println!("cargo:rerun-if-changed={}", path.display());
  }
}
