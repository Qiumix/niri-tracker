//! Configuration loading.
//!
//! The shape is deliberately small:
//!
//! - The preset is a complete TOML document compiled into the binary. It is the
//!   single source of truth for defaults, readable and editable as a file rather
//!   than scattered across `unwrap_or` calls.
//! - A user file contains only what the user wants to change. Anything absent
//!   keeps the preset value, and arrays are appended unless the user opts out.
//! - A missing user file is normal and yields the defaults. A malformed one is
//!   fatal: running with defaults while the user believes a five-minute idle
//!   timeout is in effect collects days of wrong data before anyone notices.

mod duration;
mod model;
mod paths;

pub use duration::{DurationError, format as format_duration, parse as parse_duration};
pub use model::{
    AliasConfig, Config, Fallback, IdleConfig, ReportConfig, ResolveError, Resolved, StorageConfig,
    TerminalConfig, Unmatched,
};
pub use paths::{SYSTEM_SCHEMA, config_dir, config_file, data_dir, database_file, runtime_dir};

/// The compiled-in defaults, as a complete TOML document.
pub const PRESET: &str = include_str!("../../preset/config.toml");

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The user file exists but could not be read.
    #[error("could not read {path}: {source}", path = .path.display())]
    Read {
        /// The file that could not be read.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A TOML document did not parse.
    #[error("could not parse {path}: {source}", path = .path.display())]
    Parse {
        /// The file that failed to parse.
        path: std::path::PathBuf,
        /// The TOML error, which carries a span into the file.
        #[source]
        source: Box<toml::de::Error>,
    },

    /// The merged configuration did not resolve.
    #[error("the configuration is invalid: {0}")]
    Resolve(#[from] ResolveError),
}

/// Load the configuration from the standard location.
pub fn load() -> Result<Resolved, Error> {
    load_from(&config_file())
}

/// Load from an explicit path, merging over the compiled-in preset.
///
/// Absence of the file is not an error; anything else is.
pub fn load_from(path: &std::path::Path) -> Result<Resolved, Error> {
    let preset: Config = toml::from_str(PRESET).map_err(|source| Error::Parse {
        path: "<compiled-in preset>".into(),
        source: Box::new(source),
    })?;

    let user = match std::fs::read_to_string(path) {
        Ok(text) => {
            let parsed: Config = toml::from_str(&text).map_err(|source| Error::Parse {
                path: path.to_owned(),
                source: Box::new(source),
            })?;
            Some(parsed)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(Error::Read {
                path: path.to_owned(),
                source,
            });
        }
    };

    let merged = match user {
        Some(user) => preset.merge(user),
        None => preset,
    };

    Ok(merged.resolve()?)
}

/// Parse a user-facing TOML document without touching the filesystem.
///
/// Used by tests and by `config check`.
pub fn parse_user(text: &str) -> Result<Config, Error> {
    toml::from_str(text).map_err(|source| Error::Parse {
        path: "<input>".into(),
        source: Box::new(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preset_parses_and_resolves() {
        let preset: Config = toml::from_str(PRESET).expect("preset must parse");
        let resolved = preset.resolve().expect("preset must resolve");
        assert_eq!(resolved.idle_timeout, std::time::Duration::ZERO);
        assert_eq!(resolved.retention, std::time::Duration::ZERO);
        assert!(!resolved.normalize_titles);
        assert!(resolved.strip_counters);
        assert!(resolved.merge_preset_arrays);
        assert!(!resolved.terminals.is_empty());
    }

    #[test]
    fn scalars_from_the_user_win() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            [idle]
            timeout = "5m"
            "#,
        )
        .unwrap();
        let resolved = preset.merge(user).resolve().unwrap();
        assert_eq!(resolved.idle_timeout, std::time::Duration::from_secs(300));
    }

    #[test]
    fn arrays_are_appended_by_default() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let preset_terminals = preset.terminal.as_ref().unwrap().len();
        let user = parse_user(
            r#"
            [[terminal]]
            app_ids = ["myterm"]
            "#,
        )
        .unwrap();
        let resolved = preset.merge(user).resolve().unwrap();
        assert_eq!(resolved.terminals.len(), preset_terminals + 1);
    }

    #[test]
    fn arrays_can_replace_instead() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            merge_preset_arrays = false

            [[terminal]]
            app_ids = ["myterm"]
            "#,
        )
        .unwrap();
        let resolved = preset.merge(user).resolve().unwrap();
        assert_eq!(resolved.terminals.len(), 1);
        assert_eq!(resolved.terminals[0].app_ids, vec!["myterm"]);
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = parse_user(
            r#"
            [idle]
            time_out = "5m"
            "#,
        );
        assert!(
            err.is_err(),
            "a misspelled key must not be silently ignored"
        );
    }

    #[test]
    fn a_bad_duration_names_the_field() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            [idle]
            timeout = "soon"
            "#,
        )
        .unwrap();
        let err = preset.merge(user).resolve().unwrap_err();
        let text = err.to_string();
        assert!(text.contains("idle.timeout"), "got: {text}");
    }

    #[test]
    fn duplicate_alias_names_are_rejected() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            [[alias]]
            name = "Same"
            app_ids = ["a"]

            [[alias]]
            name = "Same"
            app_ids = ["b"]
            "#,
        )
        .unwrap();
        assert!(preset.merge(user).resolve().is_err());
    }

    #[test]
    fn a_blank_alias_title_is_rejected() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            [[alias]]
            name = "Browser"
            app_ids = ["firefox"]
            titles = ["  "]
            "#,
        )
        .unwrap();
        assert!(
            preset.merge(user).resolve().is_err(),
            "a blank needle would match every title"
        );
    }

    #[test]
    fn an_empty_terminal_entry_is_rejected() {
        let preset: Config = toml::from_str(PRESET).unwrap();
        let user = parse_user(
            r#"
            [[terminal]]
            app_ids = []
            "#,
        )
        .unwrap();
        assert!(preset.merge(user).resolve().is_err());
    }
}
