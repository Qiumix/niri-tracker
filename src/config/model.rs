//! The configuration model.
//!
//! Two layers, one type. [`Config`] describes the file as written: every scalar
//! is optional, because a user file only carries the keys it wants to change.
//! The compiled-in preset is also a [`Config`], but a complete one, so merging
//! the two yields a [`Config`] where nothing is `None`.
//!
//! [`Resolved`] is what the rest of the program consumes, and it has no `Option`
//! anywhere: "was this configured?" is answered once, here, instead of being
//! re-asked at every use site as an `unwrap_or`.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::duration::{self, DurationError};

/// The configuration file, as written by the user or by the preset.
///
/// Fields are optional because the preset supplies the rest; the preset itself
/// fills all of them in. `deny_unknown_fields` is deliberate: a misspelled key
/// would otherwise be silently ignored and the setting would appear not to work.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
	/// Append user arrays to the preset's instead of replacing them.
	#[serde(default)]
	pub merge_preset_arrays: Option<bool>,

	/// Idle detection.
	#[serde(default)]
	pub idle: IdleConfig,

	/// How long data is kept.
	#[serde(default)]
	pub storage: StorageConfig,

	/// How reports are produced.
	#[serde(default)]
	pub report: ReportConfig,

	/// Terminals whose foreground process should be credited with the time.
	#[serde(default)]
	pub terminal: Option<Vec<TerminalConfig>>,

	/// Aliases that merge several app_ids into one name.
	#[serde(default)]
	pub alias: Option<Vec<AliasConfig>>,
}

/// Idle detection.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdleConfig {
	/// How long the keyboard and mouse must be untouched before time stops
	/// counting. `"0s"` disables idle detection, which is the default.
	#[serde(default)]
	pub timeout: Option<String>,
}

/// How long data is kept.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
	/// Delete sessions older than this. `"0s"` keeps everything, the default.
	#[serde(default)]
	pub retention: Option<String>,
}

/// How reports are produced.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportConfig {
	/// Apply a light normalisation to titles when grouping by title.
	#[serde(default)]
	pub normalize_titles: Option<bool>,

	/// Strip trailing counters such as `Inbox (3)`. Only used when
	/// `normalize_titles` is true.
	#[serde(default)]
	pub strip_counters: Option<bool>,
}

/// A terminal emulator whose windows should be credited to what runs inside.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
	/// app_ids this entry applies to, as niri reports them.
	pub app_ids: Vec<String>,

	/// Credit the foreground process instead of the terminal. The default is
	/// true: naming the terminal for an nvim session defeats the purpose.
	#[serde(default = "default_true")]
	pub split: bool,

	/// What to credit the time to when the foreground process cannot be read.
	#[serde(default)]
	pub fallback: Fallback,
}

/// What to credit a terminal's time to when its foreground process is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
	/// The terminal's own app_id.
	#[default]
	Terminal,
}

/// An alias that merges several app_ids into one name.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AliasConfig {
	/// The name shown in reports.
	pub name: String,

	/// app_ids folded into this name.
	pub app_ids: Vec<String>,

	/// Optional title substrings that split this app further.
	#[serde(default)]
	pub titles: Vec<String>,

	/// What to do with a listed app whose title matched no entry in `titles`.
	#[serde(default)]
	pub unmatched: Unmatched,
}

/// What to do with a listed app whose title matched no `titles` entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unmatched {
	/// Leave it under its own app_id. This is the default because folding
	/// everything into `Name (Other)` is a large, surprising reclassification.
	#[default]
	Keep,
	/// Fold it into `Name (Other)`.
	Other,
}

/// The configuration with nothing left to decide.
#[derive(Debug, Clone)]
pub struct Resolved {
	/// Whether arrays from the preset were appended to.
	pub merge_preset_arrays: bool,
	/// How long input must be idle before time stops counting. Zero disables it.
	pub idle_timeout: Duration,
	/// How long sessions are kept. Zero keeps them forever.
	pub retention: Duration,
	/// Whether titles are normalised when grouping by title.
	pub normalize_titles: bool,
	/// Whether trailing counters are stripped during normalisation.
	pub strip_counters: bool,
	/// Terminals to resolve to their foreground process.
	pub terminals: Vec<TerminalConfig>,
	/// Aliases applied when a report is generated.
	pub aliases: Vec<AliasConfig>,
}

/// A problem found while turning a merged [`Config`] into a [`Resolved`].
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
	/// A field the preset must supply was absent. This means the compiled-in
	/// preset is broken, not the user's file.
	#[error("the preset is missing `{0}`, which it must always set")]
	Missing(&'static str),

	/// A duration field could not be parsed.
	#[error("`{field}`: {source}")]
	Duration {
		/// The dotted path of the offending key.
		field: &'static str,
		/// Why it was rejected.
		#[source]
		source: DurationError,
	},

	/// A `[[terminal]]` entry had no app_ids.
	#[error("[[terminal]] entry #{index} has an empty `app_ids`, so it can never match")]
	TerminalWithoutAppIds {
		/// Its index in the list, counting from one as an editor does.
		index: usize,
	},

	/// An `[[alias]]` entry had an empty name or no app_ids.
	#[error("[[alias]] entry #{index} must have a non-empty `name` and `app_ids`")]
	AliasIncomplete {
		/// Its index in the list, counting from one.
		index: usize,
	},

	/// Two aliases shared a name.
	#[error("two [[alias]] entries are both named {0:?}")]
	AliasDuplicate(String),
}

impl Config {
	/// Merge a user file over the preset.
	///
	/// Scalars from the user win. Arrays are appended to the preset's when
	/// `merge_preset_arrays` is true, so a user adding one terminal does not
	/// silently lose the built-in list; setting the flag false replaces them
	/// wholesale.
	#[must_use]
	pub fn merge(self, user: Self) -> Self {
		let merge_arrays = user
			.merge_preset_arrays
			.or(self.merge_preset_arrays)
			.unwrap_or(false);

		Self {
			merge_preset_arrays: Some(merge_arrays),
			idle: IdleConfig {
				timeout: user.idle.timeout.or(self.idle.timeout),
			},
			storage: StorageConfig {
				retention: user.storage.retention.or(self.storage.retention),
			},
			report: ReportConfig {
				normalize_titles: user
					.report
					.normalize_titles
					.or(self.report.normalize_titles),
				strip_counters: user.report.strip_counters.or(self.report.strip_counters),
			},
			terminal: merge_arrays_of(self.terminal, user.terminal, merge_arrays),
			alias: merge_arrays_of(self.alias, user.alias, merge_arrays),
		}
	}

	/// Turn the merged configuration into one with no undecided fields.
	pub fn resolve(self) -> Result<Resolved, ResolveError> {
		let idle_timeout = parse_field("idle.timeout", self.idle.timeout)?;
		let retention = parse_field("storage.retention", self.storage.retention)?;

		let normalize_titles = self
			.report
			.normalize_titles
			.ok_or(ResolveError::Missing("report.normalize_titles"))?;
		let strip_counters = self
			.report
			.strip_counters
			.ok_or(ResolveError::Missing("report.strip_counters"))?;

		let terminals = self.terminal.unwrap_or_default();
		for (offset, entry) in terminals.iter().enumerate() {
			if entry.app_ids.is_empty() {
				return Err(ResolveError::TerminalWithoutAppIds { index: offset + 1 });
			}
		}

		let aliases = self.alias.unwrap_or_default();
		let mut seen: Vec<&str> = Vec::with_capacity(aliases.len());
		for (offset, entry) in aliases.iter().enumerate() {
			if entry.name.trim().is_empty() || entry.app_ids.is_empty() {
				return Err(ResolveError::AliasIncomplete { index: offset + 1 });
			}
			if seen.contains(&entry.name.as_str()) {
				return Err(ResolveError::AliasDuplicate(entry.name.clone()));
			}
			seen.push(&entry.name);
		}

		Ok(Resolved {
			merge_preset_arrays: self.merge_preset_arrays.unwrap_or(false),
			idle_timeout,
			retention,
			normalize_titles,
			strip_counters,
			terminals,
			aliases,
		})
	}
}

/// Merge two optional lists according to the append flag.
fn merge_arrays_of<T>(
	preset: Option<Vec<T>>,
	user: Option<Vec<T>>,
	append: bool,
) -> Option<Vec<T>> {
	match (preset, user) {
		(Some(preset), Some(user)) if append => {
			let mut merged = preset;
			merged.extend(user);
			Some(merged)
		}
		// Replacing means "the user takes full control".
		(_, Some(user)) => Some(user),
		(preset, None) => preset,
	}
}

/// Read a duration field that the preset must provide.
fn parse_field(field: &'static str, value: Option<String>) -> Result<Duration, ResolveError> {
	let text = value.ok_or(ResolveError::Missing(field))?;
	duration::parse(&text).map_err(|source| ResolveError::Duration { field, source })
}

/// `#[serde(default = "...")]` helper.
fn default_true() -> bool {
	true
}
