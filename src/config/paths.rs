//! Where niri-tracker keeps its files, following the XDG base directory spec.
//!
//! Every location can be overridden by an environment variable, which is what
//! the tests and a packaged instance use. The override wins outright; otherwise
//! the XDG variable is consulted, and `$HOME` is the last resort.

use std::path::PathBuf;

/// Where a file belongs could not be worked out.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Neither the XDG variable nor `HOME` was set.
    ///
    /// Returning a relative path here would silently anchor the files to whatever
    /// directory the process happened to start in.
    #[error("neither ${xdg} nor $HOME is set, so the {what} directory is unknown")]
    NoBase {
        /// The XDG variable that was consulted.
        xdg: &'static str,
        /// What the directory holds, for the message.
        what: &'static str,
    },
}

/// The configuration directory.
///
/// `NIRI_TRACKER_CONFIG_DIR` overrides it.
pub fn config_dir() -> Result<PathBuf, Error> {
    resolve_dir(
        std::env::var_os("NIRI_TRACKER_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        ".config",
        "XDG_CONFIG_HOME",
        "configuration",
    )
}

/// The main configuration file.
pub fn config_file() -> Result<PathBuf, Error> {
    Ok(config_dir()?.join("config.toml"))
}

/// The data directory holding the database.
///
/// `NIRI_TRACKER_DATA_DIR` overrides it.
pub fn data_dir() -> Result<PathBuf, Error> {
    resolve_dir(
        std::env::var_os("NIRI_TRACKER_DATA_DIR"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        ".local/share",
        "XDG_DATA_HOME",
        "data",
    )
}

/// The database file.
pub fn database_file() -> Result<PathBuf, Error> {
    Ok(data_dir()?.join("track.db"))
}

/// The directory for sockets and instance metadata.
///
/// `NIRI_TRACKER_RUNTIME_DIR` overrides it, which is what the tests use.
/// Prefers `XDG_RUNTIME_DIR`, which is per-user and cleared on logout; `/tmp` is
/// only a fallback for systems without one, and is still per-user because each
/// instance's files are named after its pid.
#[must_use]
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NIRI_TRACKER_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("niri-tracker");
    }
    PathBuf::from("/tmp/niri-tracker")
}

/// Where a packaged installation publishes the JSON schema.
///
/// Used only to decide whether `config init` can point `#:schema` at something
/// that exists.
pub const SYSTEM_SCHEMA: &str = "/usr/share/niri-tracker/schema.json";

/// Resolve a directory from already-fetched environment values.
///
/// Split out from [`config_dir`] and friends because the environment is
/// process-wide: tests that mutated it raced each other when cargo ran them in
/// parallel. They call this instead.
fn resolve_dir(
    override_dir: Option<std::ffi::OsString>,
    xdg_var: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    xdg_suffix: &str,
    xdg_name: &'static str,
    what: &'static str,
) -> Result<PathBuf, Error> {
    if let Some(dir) = override_dir {
        return Ok(PathBuf::from(dir));
    }
    let base = match (xdg_var, home) {
        (Some(xdg), _) => PathBuf::from(xdg),
        (None, Some(home)) => PathBuf::from(home).join(xdg_suffix),
        (None, None) => {
            return Err(Error::NoBase {
                xdg: xdg_name,
                what,
            });
        }
    };
    Ok(base.join("niri-tracker"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn the_override_wins() {
        let dir = resolve_dir(
            Some(OsString::from("/tmp/nt-cfg")),
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/user")),
            ".config",
            "XDG_CONFIG_HOME",
            "configuration",
        )
        .expect("resolves");
        assert_eq!(dir, PathBuf::from("/tmp/nt-cfg"));
    }

    #[test]
    fn xdg_is_preferred_over_home() {
        let dir = resolve_dir(
            None,
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/user")),
            ".config",
            "XDG_CONFIG_HOME",
            "configuration",
        )
        .expect("resolves");
        assert_eq!(dir, PathBuf::from("/xdg/niri-tracker"));
    }

    #[test]
    fn home_is_the_fallback() {
        let dir = resolve_dir(
            None,
            None,
            Some(OsString::from("/home/user")),
            ".config",
            "XDG_CONFIG_HOME",
            "configuration",
        )
        .expect("resolves");
        assert_eq!(dir, PathBuf::from("/home/user/.config/niri-tracker"));
    }

    #[test]
    fn no_base_at_all_is_an_error_rather_than_a_relative_path() {
        // A relative path would put the database in whatever directory the process
        // happened to start in, which is not a place anyone chose.
        let result = resolve_dir(
            None,
            None,
            None,
            ".config",
            "XDG_CONFIG_HOME",
            "configuration",
        );
        assert!(
            matches!(result, Err(Error::NoBase { .. })),
            "a missing base must be reported: {result:?}"
        );
    }
}
