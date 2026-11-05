//! niri-tracker, a screen time tracker for the niri Wayland compositor.
//!
//! The binary is both the daemon that watches the compositor and the client
//! that reports what it recorded. All state lives in one SQLite file, and the
//! daemon is its only writer; clients ask the daemon rather than reading it, so
//! a report and `status` can never disagree.

use std::io::Write as _;
use std::process::ExitCode;

use clap::Parser as _;

use niri_tracker::cli::{Cli, Command, ConfigCommand, write_completions};
use niri_tracker::config;

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
		Command::Schema => {
			let schema = schemars::schema_for!(config::Config);
			let text = serde_json::to_string_pretty(&schema)?;
			println!("{text}");
			Ok(ExitCode::SUCCESS)
		}

		Command::Completions(args) => {
			let mut stdout = std::io::stdout().lock();
			write_completions(args.shell, &mut stdout);
			stdout.flush()?;
			Ok(ExitCode::SUCCESS)
		}

		Command::Config(ConfigCommand::Check) => {
			let resolved = config::load_from(&config_path)?;
			println!(
				"configuration at {} is valid; idle timeout is {}",
				config_path.display(),
				config::format_duration(resolved.idle_timeout)
			);
			Ok(ExitCode::SUCCESS)
		}

		Command::Config(ConfigCommand::Init(args)) => {
			init_config(args.path.as_deref().unwrap_or(&config_path), args.force)
		}

		// Everything else needs the daemon or the database, and lands in the
		// next increment.
		other => {
			let _ = other;
			Err("this subcommand is not implemented yet".into())
		}
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
/// Nothing is guessed: a path is only written when that file is really there, so
/// a linter never reports a dangling reference.
fn schema_header() -> String {
	if std::path::Path::new(config::SYSTEM_SCHEMA).is_file() {
		return format!("#:schema {}\n\n", config::SYSTEM_SCHEMA);
	}
	if std::path::Path::new("schema.json").is_file() {
		return String::from("#:schema ./schema.json\n\n");
	}
	String::new()
}
