//! Where niri-tracker keeps its files, following the XDG base directory spec.
//!
//! Every location can be overridden by an environment variable, which is what
//! the tests and a packaged instance use. The override wins outright; otherwise
//! the XDG variable is consulted, and `$HOME` is the last resort.

use std::path::PathBuf;

/// The configuration directory.
///
/// `NIRI_TRACKER_CONFIG_DIR` overrides it.
#[must_use]
pub fn config_dir() -> PathBuf {
	resolve_dir(
		std::env::var_os("NIRI_TRACKER_CONFIG_DIR"),
		std::env::var_os("XDG_CONFIG_HOME"),
		std::env::var_os("HOME"),
		".config",
	)
}

/// The main configuration file.
#[must_use]
pub fn config_file() -> PathBuf {
	config_dir().join("config.toml")
}

/// The data directory holding the database.
///
/// `NIRI_TRACKER_DATA_DIR` overrides it.
#[must_use]
pub fn data_dir() -> PathBuf {
	resolve_dir(
		std::env::var_os("NIRI_TRACKER_DATA_DIR"),
		std::env::var_os("XDG_DATA_HOME"),
		std::env::var_os("HOME"),
		".local/share",
	)
}

/// The database file.
#[must_use]
pub fn database_file() -> PathBuf {
	data_dir().join("track.db")
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
#[must_use]
fn resolve_dir(
	override_dir: Option<std::ffi::OsString>,
	xdg_var: Option<std::ffi::OsString>,
	home: Option<std::ffi::OsString>,
	xdg_suffix: &str,
) -> PathBuf {
	if let Some(dir) = override_dir {
		return PathBuf::from(dir);
	}
	let base = xdg_var.map_or_else(
		|| PathBuf::from(home.unwrap_or_default()).join(xdg_suffix),
		PathBuf::from,
	);
	base.join("niri-tracker")
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
		);
		assert_eq!(dir, PathBuf::from("/tmp/nt-cfg"));
	}

	#[test]
	fn xdg_is_preferred_over_home() {
		let dir = resolve_dir(
			None,
			Some(OsString::from("/xdg")),
			Some(OsString::from("/home/user")),
			".config",
		);
		assert_eq!(dir, PathBuf::from("/xdg/niri-tracker"));
	}

	#[test]
	fn home_is_the_fallback() {
		let dir = resolve_dir(None, None, Some(OsString::from("/home/user")), ".config");
		assert_eq!(dir, PathBuf::from("/home/user/.config/niri-tracker"));
	}
}
