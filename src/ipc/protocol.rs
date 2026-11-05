//! The wire format for the control protocol.
//!
//! One JSON object per line, one request per connection: `{id, verb, args}` out,
//! `{id, result}` or `{id, error}` back. Newline framing is deliberate, because
//! `nc -U` piped into `jq` is enough to poke the daemon by hand, which matters
//! more than saving bytes on a socket that sees a few messages a minute.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::db::Live;

/// A client request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
	/// Client-assigned correlation id, echoed in the response.
	pub id: u64,
	/// The operation to perform.
	pub verb: Verb,
	/// Verb-specific payload. Absent for verbs that take no arguments.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub args: Option<Args>,
}

/// Operations the daemon understands.
///
/// An enum rather than a string, so an unknown verb is rejected at parse time
/// with a message naming the valid variants instead of travelling to the daemon
/// and coming back as a string mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
	/// Report what the daemon is doing right now.
	Status,
	/// Stop recording until `resume`.
	Pause,
	/// Resume recording.
	Resume,
	/// Re-read the configuration.
	Reload,
	/// Stop the daemon.
	Quit,
	/// Total time per application.
	Report,
	/// Window titles of one application.
	Details,
}

/// Arguments for verbs that take them.
///
/// `untagged` so the wire stays terse: a filter is an object, and a verb with no
/// arguments simply omits `args`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Args {
	/// A range and grouping, for `report` and `details`.
	Query(QueryArgs),
}

/// What a report or details request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryArgs {
	/// First day to include.
	pub from: NaiveDate,
	/// Last day to include, inclusive.
	pub to: NaiveDate,
	/// What the rows are grouped by.
	pub by: GroupKey,
	/// Only count this app_id, matched exactly.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub app: Option<String>,
	/// Only count titles containing this text.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	/// Leave out time an idle inhibitor covered.
	#[serde(default)]
	pub exclude_passive: bool,
	/// Keep at most this many rows. Totals still cover everything counted.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit: Option<usize>,
}

/// How report rows are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKey {
	/// By application.
	App,
	/// By window title.
	Title,
	/// By day.
	Day,
	/// By hour of the day.
	Hour,
}

/// A server response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
	/// The verb succeeded.
	Ok {
		/// Echo of the request id.
		id: u64,
		/// Verb-specific payload.
		result: Payload,
	},
	/// The verb failed.
	Failed {
		/// Echo of the request id.
		id: u64,
		/// What went wrong.
		error: WireError,
	},
}

impl Response {
	/// The request this answers.
	#[must_use]
	pub fn id(&self) -> u64 {
		match self {
			Self::Ok { id, .. } | Self::Failed { id, .. } => *id,
		}
	}
}

/// Successful payloads.
///
/// `untagged`, so ordering matters: a value that could match two variants takes
/// the first. `Status` and `Report` are distinguished by their required fields,
/// which no other variant supplies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Payload {
	/// A snapshot of the daemon.
	Status(Status),
	/// Grouped rows.
	Report(Report),
	/// Nothing to report beyond success, encoded as `null`.
	Empty(Empty),
}

/// What the daemon is doing right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
	/// Whether recording is currently paused.
	pub paused: bool,
	/// The window being timed, absent when nothing is focused or recording is
	/// paused.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub live: Option<LiveStatus>,
	/// The daemon's process id, so a client can tell instances apart.
	pub pid: u32,
	/// How many intervals the database holds, as a cheap sanity check.
	pub intervals: u64,
}

/// The stretch currently being recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveStatus {
	/// The application.
	pub app_id: String,
	/// Its window title.
	pub title: String,
	/// How long it has been running, in whole seconds.
	pub seconds: i64,
	/// Whether an idle inhibitor covers it.
	pub passive: bool,
}

impl From<&Live> for LiveStatus {
	fn from(live: &Live) -> Self {
		Self {
			app_id: live.app_id.clone(),
			title: live.title.clone(),
			seconds: 0,
			passive: live.passive,
		}
	}
}

/// Grouped rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
	/// One row per group, largest first.
	pub rows: Vec<Row>,
	/// The sum of every row, before any limit was applied.
	pub total_seconds: i64,
}

/// One group of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
	/// The group's label: an app_id, a title, a date or an hour.
	pub name: String,
	/// How long it accumulated.
	pub seconds: i64,
}

/// The acknowledgement for a verb that changes something, encoded as `null`.
///
/// A unit struct serialises to `null`, which is what distinguishes it from the
/// structured payloads under `untagged`: an empty object would also satisfy a
/// variant whose fields are all optional, so `null` is the one encoding no other
/// variant can claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty;

/// A failure, in a form a client can relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
	/// A short machine-stable tag.
	pub kind: ErrorKind,
	/// A sentence for a human.
	pub message: String,
}

/// What kind of failure a response carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
	/// The request did not make sense.
	BadRequest,
	/// The daemon could not do it right now.
	Unavailable,
	/// The database refused.
	Storage,
	/// Something unexpected.
	Internal,
}

/// Serialize a message as one line, newline included.
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
	let mut text = serde_json::to_string(value)?;
	text.push('\n');
	Ok(text)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn date(day: u32) -> NaiveDate {
		NaiveDate::from_ymd_opt(2026, 11, day).expect("valid date")
	}

	#[test]
	fn a_request_round_trips() {
		let request = Request {
			id: 7,
			verb: Verb::Report,
			args: Some(Args::Query(QueryArgs {
				from: date(1),
				to: date(5),
				by: GroupKey::App,
				app: None,
				title: None,
				exclude_passive: false,
				limit: None,
			})),
		};

		let line = encode_line(&request).expect("encodes");
		assert!(line.ends_with('\n'));
		assert_eq!(line.matches('\n').count(), 1, "one line only");

		let parsed: Request = serde_json::from_str(&line).expect("decodes");
		assert_eq!(parsed, request);
	}

	#[test]
	fn a_bare_verb_omits_args() {
		let request = Request {
			id: 1,
			verb: Verb::Status,
			args: None,
		};
		let line = encode_line(&request).expect("encodes");
		assert!(!line.contains("args"), "got: {line}");
	}

	#[test]
	fn an_unknown_verb_is_refused_at_parse_time() {
		let text = r#"{"id":1,"verb":"teleport"}"#;
		let err = serde_json::from_str::<Request>(text).expect_err("must not parse");
		assert!(err.to_string().contains("teleport"), "got: {err}");
	}

	#[test]
	fn a_report_response_round_trips() {
		let response = Response::Ok {
			id: 3,
			result: Payload::Report(Report {
				rows: vec![Row {
					name: String::from("kitty"),
					seconds: 90,
				}],
				total_seconds: 90,
			}),
		};
		let line = encode_line(&response).expect("encodes");
		let parsed: Response = serde_json::from_str(&line).expect("decodes");
		assert_eq!(parsed, response);
		assert_eq!(parsed.id(), 3);
	}

	#[test]
	fn a_status_response_round_trips() {
		let response = Response::Ok {
			id: 1,
			result: Payload::Status(Status {
				paused: false,
				live: None,
				pid: 42,
				intervals: 7,
			}),
		};
		let line = encode_line(&response).expect("encodes");
		let parsed: Response = serde_json::from_str(&line).expect("decodes");
		assert_eq!(parsed, response);
	}

	#[test]
	fn an_error_response_round_trips() {
		let response = Response::Failed {
			id: 9,
			error: WireError {
				kind: ErrorKind::BadRequest,
				message: String::from("no such app"),
			},
		};
		let line = encode_line(&response).expect("encodes");
		let parsed: Response = serde_json::from_str(&line).expect("decodes");
		assert_eq!(parsed, response);
	}

	#[test]
	fn an_acknowledgement_is_null() {
		let response = Response::Ok {
			id: 2,
			result: Payload::Empty(Empty),
		};
		let line = encode_line(&response).expect("encodes");
		assert!(line.contains("null"), "got: {line}");
	}
}
