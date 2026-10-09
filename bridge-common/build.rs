//! Stamp the library with the Omniphony commit its `bridge_api` came from, so
//! a bridge that will not load can be matched against the host (#121).
//!
//! The commit is `HARLETTY_OMNIPHONY_COMMIT` when set (the release workflow
//! sets it to the commit `.omniphony-ref` resolves to), else `git rev-parse
//! HEAD` in the sibling checkout the path dependencies point at
//! (`../../Omniphony` from here), else `unknown` (a source archive, no git).
//!
//! Re-run only when that answer can change: this file, the variable, and the
//! checkout's `HEAD`, its reflog and the branch it names. Nothing else, so an
//! ordinary build never re-runs it.

use std::path::{Path, PathBuf};
use std::process::Command;

const COMMIT_VAR: &str = "HARLETTY_OMNIPHONY_COMMIT";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={COMMIT_VAR}");

    let commit = match std::env::var(COMMIT_VAR) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
        _ => {
            let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
            git_head(&manifest_dir.join("../../Omniphony")).unwrap_or_else(|| "unknown".to_owned())
        }
    };
    println!("cargo:rustc-env=HARLETTY_BUILD_OMNIPHONY_COMMIT={commit}");
}

/// `HEAD` of the checkout at `dir`, if `dir` is the top of one, with the files
/// to watch for it to move.
fn git_head(dir: &Path) -> Option<String> {
    let dir = dir.canonicalize().ok()?;
    // A plain directory inside some other repository is not a checkout of
    // Omniphony: its enclosing repository's HEAD would be a wrong answer.
    let top = git(&dir, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&top).canonicalize().ok()? != dir {
        return None;
    }
    let commit = git(&dir, &["rev-parse", "--verify", "HEAD^{commit}"])?;

    // `--git-path` resolves worktrees (HEAD in the worktree's own git dir, refs
    // in the common one). HEAD's reflog is appended whenever HEAD moves, even
    // through a branch whose ref lives only in packed-refs. Only existing paths
    // are watched: Cargo treats a missing one as changed and would re-run on
    // every build.
    let mut watched = vec![git_path(&dir, "HEAD"), git_path(&dir, "logs/HEAD")];
    if let Some(branch) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
        watched.push(git_path(&dir, &branch));
    }
    for path in watched.into_iter().flatten().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Some(commit)
}

fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git(dir, &["rev-parse", "--git-path", name])?);
    Some(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// The trimmed stdout of `git -C dir args..`, `None` when it fails or prints
/// nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // A build run from a git hook inherits these, and they would point
        // every command at this repository instead.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
