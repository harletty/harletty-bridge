// SPDX-License-Identifier: Apache-2.0
//! Local settings: what a job needs that is neither in the repository nor in
//! the input.
//!
//! One YAML file, outside the source tree, holding what belongs to the
//! machine rather than to the project. Today that is one thing — the key an
//! Evolution frame's protection word is signed with, which is not this
//! project's to ship and which this project neither distributes nor helps
//! anyone obtain. Without it a stream's signature is simply not checked.
//!
//! Read from, in order:
//!
//! 1. the path given on the command line (`--config`);
//! 2. `$HARLETTY_CONFIG`;
//! 3. `$XDG_CONFIG_HOME/harletty/config.yaml`;
//! 4. `~/.config/harletty/config.yaml`;
//! 5. the same two default locations under `harlettizer/`.
//!
//! An explicit path that does not exist is an error; a default one that does
//! not exist is simply no settings. Step 5 is there because the encoder in
//! that project reads the same key from the same field, and one key on a
//! machine is one file: whoever has already set it up for the encoder does
//! not set it up twice. Its file is read leniently — a field this tool does
//! not know belongs to that one — while a file of our own is read strictly,
//! because a key whose name is misspelt and silently dropped is a stream
//! reported unchecked when it could have been checked.
//!
//! ```yaml
//! # ~/.config/harletty/config.yaml — local to this machine, mode 600.
//! evolution_key: "0001…"   # hexadecimal, 32 bytes for the key streams carry
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// The file's name under its directory.
const FILE: &str = "config.yaml";

/// The environment variable that names the file outright.
const ENV: &str = "HARLETTY_CONFIG";

/// This tool's configuration directory, and the encoder's, in the order they
/// are looked for.
const DIRECTORIES: [&str; 2] = ["harletty", "harlettizer"];

/// What the file says, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The key Evolution frames' protection words are signed with.
    pub evolution_key: Option<Vec<u8>>,
    /// Where it was read from, when it was.
    pub from: Option<PathBuf>,
}

/// The file's shape as written, for a file of ours: nothing but the fields
/// below, so that a misspelt key is refused rather than ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Strict {
    #[serde(default)]
    evolution_key: Option<String>,
}

/// The same fields, for another project's file: what it carries beyond them
/// is its own business.
#[derive(Deserialize)]
struct Lenient {
    #[serde(default)]
    evolution_key: Option<String>,
}

impl Settings {
    /// Where the settings would be read from, given what the command line
    /// said: the first location in the order above that exists, or, for an
    /// explicit path, that path whether it exists or not.
    pub fn location(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_path_buf());
        }
        if let Some(path) = std::env::var_os(ENV).filter(|path| !path.is_empty()) {
            return Some(PathBuf::from(path));
        }
        Self::defaults().into_iter().find(|path| path.is_file())
    }

    /// The default locations, in order: ours under both roots, then the
    /// encoder's.
    fn defaults() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME").filter(|root| !root.is_empty()) {
            roots.push(PathBuf::from(root));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            roots.push(PathBuf::from(home).join(".config"));
        }
        DIRECTORIES
            .iter()
            .flat_map(|directory| {
                roots
                    .iter()
                    .map(move |root| root.join(directory).join(FILE))
            })
            .collect()
    }

    /// Read the settings, from the explicit path or the first default one
    /// that exists.
    ///
    /// A default file that is absent is no settings and not an error; a path
    /// that was asked for by name has to exist.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let Some(path) = Self::location(explicit) else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                return Ok(Self::default());
            }
            Err(why) => return Err(anyhow!(why)).context(format!("reading {}", path.display())),
        };
        // Only a file of ours is held to knowing every field it carries.
        let ours = explicit.is_some()
            || std::env::var_os(ENV).is_some_and(|named| !named.is_empty())
            || path.parent().and_then(Path::file_name)
                == Some(std::ffi::OsStr::new(DIRECTORIES[0]));
        let mut settings = Self::parse(&text, ours)
            .map_err(|why| anyhow!("{}: {why}", path.display()))
            .context("reading the local settings")?;
        settings.from = Some(path);
        Ok(settings)
    }

    /// Decode the file's text. `strict` refuses a field this tool does not
    /// know; lenient passes it by.
    pub fn parse(text: &str, strict: bool) -> std::result::Result<Self, String> {
        let hex = if strict {
            serde_yaml_ng::from_str::<Strict>(text)
                .map_err(|why| why.to_string())?
                .evolution_key
        } else {
            serde_yaml_ng::from_str::<Lenient>(text)
                .map_err(|why| why.to_string())?
                .evolution_key
        };
        let evolution_key = hex
            .map(|hex| unhex(&hex).map_err(|why| format!("evolution_key: {why}")))
            .transpose()?;
        Ok(Self {
            evolution_key,
            from: None,
        })
    }
}

/// Hexadecimal, whitespace allowed anywhere, to bytes.
fn unhex(text: &str) -> std::result::Result<Vec<u8>, String> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| {
            (byte as char)
                .to_digit(16)
                .map(|digit| digit as u8)
                .ok_or_else(|| format!("`{}` is not a hexadecimal digit", byte as char))
        })
        .collect::<std::result::Result<_, _>>()?;
    if digits.is_empty() {
        return Err("empty".into());
    }
    if digits.len() % 2 != 0 {
        return Err(format!(
            "{} hexadecimal digits is not a whole number of bytes",
            digits.len()
        ));
    }
    Ok(digits
        .chunks_exact(2)
        .map(|pair| pair[0] << 4 | pair[1])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_reads_as_bytes_whatever_its_spacing() {
        let settings = Settings::parse("evolution_key: \"00 01 0a\\n ff\"\n", true).unwrap();
        assert_eq!(settings.evolution_key, Some(vec![0x00, 0x01, 0x0a, 0xff]));
        let settings = Settings::parse("evolution_key: 0001FF\n", true).unwrap();
        assert_eq!(settings.evolution_key, Some(vec![0x00, 0x01, 0xff]));
    }

    #[test]
    fn an_empty_file_is_no_settings() {
        assert_eq!(Settings::parse("", true).unwrap(), Settings::default());
        assert_eq!(
            Settings::parse("# only a comment\n", true).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn a_malformed_key_is_refused_and_says_why() {
        let why = Settings::parse("evolution_key: abc\n", true).unwrap_err();
        assert!(why.contains("evolution_key"), "{why}");
        assert!(why.contains("whole number of bytes"), "{why}");
        let why = Settings::parse("evolution_key: xyz0\n", true).unwrap_err();
        assert!(why.contains("not a hexadecimal digit"), "{why}");
        let why = Settings::parse("evolution_key: \"\"\n", true).unwrap_err();
        assert!(why.contains("empty"), "{why}");
    }

    /// A key spelt wrong in our own file is a stream reported unchecked; the
    /// file refuses it. The encoder's file is another project's, and a field
    /// of its own is none of our business.
    #[test]
    fn an_unknown_field_is_refused_in_our_file_and_passed_by_in_the_other() {
        let text = "evolution_key: 00\nsomething_else: 1\n";
        assert!(Settings::parse(text, true).is_err());
        assert_eq!(
            Settings::parse(text, false).unwrap().evolution_key,
            Some(vec![0])
        );
        assert!(Settings::parse("evolution-key: 00\n", true).is_err());
    }

    #[test]
    fn an_explicit_path_wins_and_has_to_exist() {
        let path = Path::new("/nonexistent/harletty/config.yaml");
        assert_eq!(Settings::location(Some(path)), Some(path.to_path_buf()));
        assert!(Settings::load(Some(path)).is_err());
    }

    /// Ours is looked for before the encoder's, and under `XDG_CONFIG_HOME`
    /// before `HOME`.
    #[test]
    fn the_default_locations_are_in_order() {
        // The process's own environment is what `defaults` reads, and the
        // test binary is single-threaded in neither sense; this only checks
        // the shape of what it returns for the environment it is run in.
        let defaults = Settings::defaults();
        let names: Vec<String> = defaults
            .iter()
            .map(|path| {
                path.parent()
                    .and_then(Path::file_name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let ours = names.iter().position(|name| name == DIRECTORIES[0]);
        let theirs = names.iter().position(|name| name == DIRECTORIES[1]);
        match (ours, theirs) {
            (Some(ours), Some(theirs)) => assert!(ours < theirs, "{names:?}"),
            // No HOME and no XDG_CONFIG_HOME: nothing to order.
            _ => assert!(defaults.is_empty(), "{names:?}"),
        }
    }
}
