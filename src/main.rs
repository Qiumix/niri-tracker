//! niri-tracker, a screen time tracker for the niri Wayland compositor.
//!
//! The binary is both the daemon that watches the compositor and the client that
//! reports what it recorded. All state lives in one SQLite file, and the daemon
//! is its only writer; clients ask the daemon rather than reading it, so a report
//! and `status` can never disagree.

use std::io::Write as _;
use std::process::ExitCode;

use chrono::Local;
use clap::Parser as _;

use niri_tracker::cli::{
	Cli, Command, CompletionArgs, ConfigCommand, DetailsArgs, ReportArgs, write_completions,
};
use niri_tracker::config;
use niri_tracker::ipc::{Args, Client, GroupKey, Payload, QueryArgs, Report, Verb};

fn main() -> ExitCode {
	let cli = Cli::parse();
	match run(cli) {
		Ok(code) => code,
		Err(err) => {
			eprintln!("niri-tracker: {err}");
			let mut source = std::error::Error::source(&*err);
			while let Some(cause) = source {
				eprintln!("  caused by: {cause}");
				source = cause.source();
			}
			ExitCode::FAILURE
		}
	}
}

type BoxError = Box<dyn std::error::Error>;

fn run(cli: Cli) -> Result<ExitCode, BoxError> {
	let config_path = cli.config.clone().unwrap_or_else(config::config_file);

	match cli.command {
		Command::Daemon => {
			let resolved = config::load_from(&config_path)?;
			niri_tracker::daemon::run(&resolved, &config_path)?;
			Ok(ExitCode::SUCCESS)
		}

		Command::Report(args) => report(&config_path, args),
		Command::Details(args) => details(&config_path, args),

		Command::Status => {
			let response = client()?.simple(Verb::Status)?;
			match response {
				niri_tracker::ipc::Response::Ok {
					result: Payload::Status(status),
					..
				} => {
					print_status(&status);
					Ok(ExitCode::SUCCESS)
				}
				other => Err(format!("unexpected reply: {other:?}").into()),
			}
		}

		Command::Pause => acknowledge(client()?.simple(Verb::Pause)?),
		Command::Resume => acknowledge(client()?.simple(Verb::Resume)?),
		Command::Reload => acknowledge(client()?.simple(Verb::Reload)?),
		Command::Quit => acknowledge(client()?.simple(Verb::Quit)?),

		Command::Schema => {
			let schema = schemars::schema_for!(config::Config);
			println!("{}", serde_json::to_string_pretty(&schema)?);
			Ok(ExitCode::SUCCESS)
		}

		Command::Completions(args) => completions(args),

		Command::Config(ConfigCommand::Check) => {
			let resolved = config::load_from(&config_path)?;
			println!(
				"{} is valid; idle timeout is {}",
				config_path.display(),
				config::format_duration(resolved.idle_timeout)
			);
			Ok(ExitCode::SUCCESS)
		}

		Command::Config(ConfigCommand::Init(args)) => {
			init_config(args.path.as_deref().unwrap_or(&config_path), args.force)
		}
	}
}

/// A connection to the daemon for this session.
///
/// Failing here is a hard error rather than a fall back to reading the database
/// directly: a report computed without the daemon would silently leave out the
/// interval being recorded right now, and a number that is quietly wrong is worse
/// than being told to start the daemon.
fn client() -> Result<Client, BoxError> {
	let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| String::from("wayland-0"));
	let runtime_dir = config::runtime_dir();

	let found = niri_tracker::ipc::discover(&runtime_dir, &display);
	let alive: Vec<_> = found.into_iter().filter(|entry| entry.alive).collect();

	match alive.as_slice() {
		[] => Err(format!(
			"no running niri-tracker daemon for this session; start one with `niri-tracker daemon`\n  \
			 (looked for sockets in {})",
			runtime_dir.display()
		)
		.into()),
		[only] => Ok(Client::connect(&only.info.socket)?),
		[first, rest @ ..] => {
			// More than one should be impossible because the daemon refuses to be
			// the second, but a leftover from a crash can look alive briefly. The
			// newest is the right one to ask.
			let newest = alive
				.iter()
				.max_by_key(|entry| entry.info.started)
				.unwrap_or(first);
			eprintln!(
				"niri-tracker: warning: {} daemons found; using pid {}",
				rest.len() + 1,
				newest.info.pid
			);
			Ok(Client::connect(&newest.info.socket)?)
		}
	}
}

/// A report, either as a table or as JSON.
fn report(config_path: &std::path::Path, args: ReportArgs) -> Result<ExitCode, BoxError> {
	// The configuration is read for the alias rules, not for a daemon setting: a
	// report is shaped by the user's aliases even though the daemon does the work.
	let _ = config::load_from(config_path)?;

	let query = query_args(
		args.from,
		args.to,
		group_key(args.by),
		args.app,
		args.title,
		args.exclude_passive,
		args.limit,
	);
	let response = client()?.query(Verb::Report, Args::Query(query))?;
	render(response, args.json)
}

/// Window titles of one application.
fn details(_config_path: &std::path::Path, args: DetailsArgs) -> Result<ExitCode, BoxError> {
	let query = query_args(
		args.from,
		args.to,
		GroupKey::Title,
		Some(args.app),
		args.title,
		false,
		args.limit,
	);
	let response = client()?.query(Verb::Details, Args::Query(query))?;
	render(response, args.json)
}

/// Assemble a query, turning the inclusive dates into a range.
///
/// `--to` means the whole of that day, which is why it is passed through as a
/// date rather than as a midnight instant: the daemon compares local days, so a
/// range is inclusive by construction and an off-by-one is not expressible.
fn query_args(
	from: Option<chrono::NaiveDate>,
	to: Option<chrono::NaiveDate>,
	by: GroupKey,
	app: Option<String>,
	title: Option<String>,
	exclude_passive: bool,
	limit: Option<usize>,
) -> QueryArgs {
	let today = Local::now().date_naive();
	QueryArgs {
		from: from.unwrap_or(today),
		to: to.unwrap_or(today),
		by,
		app,
		title,
		exclude_passive,
		limit,
	}
}

/// Map the command line's grouping onto the wire's.
fn group_key(by: niri_tracker::cli::GroupBy) -> GroupKey {
	match by {
		niri_tracker::cli::GroupBy::App => GroupKey::App,
		niri_tracker::cli::GroupBy::Title => GroupKey::Title,
		niri_tracker::cli::GroupBy::Day => GroupKey::Day,
		niri_tracker::cli::GroupBy::Hour => GroupKey::Hour,
	}
}

/// Print a report.
fn render(response: niri_tracker::ipc::Response, json: bool) -> Result<ExitCode, BoxError> {
	let niri_tracker::ipc::Response::Ok {
		result: Payload::Report(report),
		..
	} = response
	else {
		return Err(format!("unexpected reply: {response:?}").into());
	};

	if json {
		println!("{}", serde_json::to_string_pretty(&report)?);
		return Ok(ExitCode::SUCCESS);
	}

	print_report(&report);
	Ok(ExitCode::SUCCESS)
}

/// Print a report as a table.
fn print_report(report: &Report) {
	let width = report
		.rows
		.iter()
		.map(|row| row.name.chars().count())
		.max()
		.unwrap_or(0)
		.clamp(8, 80);

	for row in &report.rows {
		let name = truncate(&row.name, width);
		println!(
			"{name:<width$}  {}",
			config::format_duration(std::time::Duration::from_secs(row.seconds.unsigned_abs())),
			width = width
		);
	}

	if report.rows.is_empty() {
		println!("(nothing recorded for this period)");
		return;
	}

	println!();
	println!(
		"{:<width$}  {}",
		"Total",
		config::format_duration(std::time::Duration::from_secs(
			report.total_seconds.unsigned_abs()
		)),
		width = width
	);
}

/// Shorten a label to `width` characters, marking the cut.
fn truncate(text: &str, width: usize) -> String {
	if text.chars().count() <= width {
		return text.to_owned();
	}
	let mut shortened: String = text.chars().take(width.saturating_sub(1)).collect();
	shortened.push('…');
	shortened
}

/// Print what the daemon is doing.
fn print_status(status: &niri_tracker::ipc::Status) {
	println!("daemon:    pid {}", status.pid);
	println!(
		"recording: {}",
		if status.paused { "paused" } else { "yes" }
	);
	println!("intervals: {}", status.intervals);

	match &status.live {
		Some(live) => {
			println!(
				"current:   {} ({}){}",
				live.app_id,
				config::format_duration(std::time::Duration::from_secs(
					live.seconds.unsigned_abs()
				)),
				if live.passive { ", passive" } else { "" }
			);
			if !live.title.is_empty() {
				println!("title:     {}", live.title);
			}
		}
		None => println!("current:   nothing focused"),
	}
}

/// Confirm that a request that changes something was accepted.
fn acknowledge(response: niri_tracker::ipc::Response) -> Result<ExitCode, BoxError> {
	match response {
		niri_tracker::ipc::Response::Ok { .. } => Ok(ExitCode::SUCCESS),
		other => Err(format!("unexpected reply: {other:?}").into()),
	}
}

/// Write a completion script for one shell.
///
/// A reader that stops early — `| head`, `| less` then quit — closes the pipe,
/// and that is a normal way to read the output rather than a failure. It is
/// reported as success so a pipeline does not look broken.
fn completions(args: CompletionArgs) -> Result<ExitCode, BoxError> {
	let mut stdout = std::io::stdout().lock();
	match write_completions(args.shell, &mut stdout).and_then(|()| stdout.flush()) {
		Ok(()) => Ok(ExitCode::SUCCESS),
		Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
		Err(err) => Err(err.into()),
	}
}

/// Write a commented configuration file, refusing to clobber one by default.
fn init_config(path: &std::path::Path, force: bool) -> Result<ExitCode, BoxError> {
	if path.exists() && !force {
		return Err(format!(
			"{} already exists; pass --force to overwrite it",
			path.display()
		)
		.into());
	}

	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent)?;
	}

	let header = schema_header();
	std::fs::write(path, format!("{header}{}", config::PRESET))?;
	println!("wrote {}", path.display());

	if header.is_empty() {
		eprintln!(
			"note: no JSON schema found, so the file has no `#:schema` line; \
			 run `niri-tracker schema > schema.json` to create one"
		);
	}

	Ok(ExitCode::SUCCESS)
}

/// The `#:schema` line taplo reads, pointing at the first schema that exists.
///
/// Nothing is guessed: a path is only written when that file is really there, so a
/// linter never reports a dangling reference.
fn schema_header() -> String {
	if std::path::Path::new(config::SYSTEM_SCHEMA).is_file() {
		return format!("#:schema {}\n\n", config::SYSTEM_SCHEMA);
	}
	if std::path::Path::new("schema.json").is_file() {
		return String::from("#:schema ./schema.json\n\n");
	}
	String::new()
}
