//! The command line.
//!
//! One binary, several jobs: `daemon` records, `report` and `details` ask the
//! daemon what it recorded, and the remaining subcommands are maintenance
//! (`schema`, `completions`, `config`).

use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};

/// Screen time tracking for the niri Wayland compositor.
#[derive(Debug, Parser)]
#[command(name = "niri-tracker", version, about, long_about = None)]
pub struct Cli {
    /// The configuration file to use instead of the standard location.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// What to do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record screen time in the background.
    ///
    /// One daemon per Wayland session; starting a second one in the same
    /// session is refused.
    Daemon,

    /// Total time per application.
    Report(ReportArgs),

    /// Window titles of one application, as separate rows.
    Details(DetailsArgs),

    /// Ask a running daemon what it is doing right now.
    Status,

    /// Stop recording until `resume`.
    Pause,

    /// Resume recording.
    Resume,

    /// Re-read the configuration without restarting.
    Reload,

    /// Ask the daemon to exit.
    Quit,

    /// Print the JSON schema for the configuration file.
    Schema,

    /// Print a shell completion script.
    Completions(CompletionArgs),

    /// Work with the configuration file.
    #[command(subcommand)]
    Config(ConfigCommand),
}

/// Arguments for `report`.
#[derive(Debug, Parser)]
pub struct ReportArgs {
    /// First day to include, as YYYY-MM-DD. Defaults to today.
    #[arg(long, value_name = "DATE")]
    pub from: Option<NaiveDate>,

    /// Last day to include, as YYYY-MM-DD. Defaults to today, and is inclusive:
    /// `--to 2026-11-05` covers all of that day.
    #[arg(long, value_name = "DATE")]
    pub to: Option<NaiveDate>,

    /// What to group by.
    #[arg(long, value_enum, default_value_t = GroupBy::App)]
    pub by: GroupBy,

    /// Only count time on this application, matched exactly against app_id.
    #[arg(long, value_name = "APP_ID")]
    pub app: Option<String>,

    /// Only count time whose window title contains this text.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// Leave out time that an idle inhibitor covered, such as a playing video.
    #[arg(long)]
    pub exclude_passive: bool,

    /// Show at most this many rows. Totals still cover everything counted.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// What `report` groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GroupBy {
    /// Application.
    App,
    /// Window title.
    Title,
    /// Day.
    Day,
    /// Hour of the day.
    Hour,
}

/// Arguments for `details`.
#[derive(Debug, Parser)]
pub struct DetailsArgs {
    /// The application to break down, matched exactly against app_id.
    #[arg(long, value_name = "APP_ID")]
    pub app: String,

    /// Only count titles containing this text.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// First day to include, as YYYY-MM-DD. Defaults to today.
    #[arg(long, value_name = "DATE")]
    pub from: Option<NaiveDate>,

    /// Last day to include, inclusive. Defaults to today.
    #[arg(long, value_name = "DATE")]
    pub to: Option<NaiveDate>,

    /// Show at most this many rows. Totals still cover everything counted.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `completions`.
#[derive(Debug, Parser)]
pub struct CompletionArgs {
    /// The shell to generate a script for.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// The shells a completion script can be generated for.
///
/// Mirrors `clap_complete::Shell`, minus the variants we do not install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    /// Bourne Again SHell.
    Bash,
    /// Friendly Interactive SHell.
    Fish,
    /// Z SHell.
    Zsh,
}

impl From<Shell> for clap_complete::Shell {
    fn from(value: Shell) -> Self {
        match value {
            Shell::Bash => Self::Bash,
            Shell::Fish => Self::Fish,
            Shell::Zsh => Self::Zsh,
        }
    }
}

/// What to do with the configuration file.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a commented configuration file pointing at the schema.
    ///
    /// Refuses to overwrite an existing file.
    Init(ConfigInitArgs),

    /// Parse the configuration and report any problem, without starting.
    Check,
}

/// Arguments for `config init`.
#[derive(Debug, Parser)]
pub struct ConfigInitArgs {
    /// Where to write it. Defaults to the standard configuration location.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Overwrite an existing file.
    #[arg(long)]
    pub force: bool,
}

/// Build the command definition, for parsing and for completions alike.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}

/// Write a completion script for `shell` into `out`.
///
/// The program name is passed explicitly rather than taken from the crate name:
/// the two happen to agree today, and a rename should not silently produce a
/// completion script that completes a command nobody has.
///
/// Errors are returned rather than panicking, which is what `clap_complete`'s
/// own `generate` does. A completion script is usually piped into `head`, `less`
/// or a file, and a reader that stops early closes the pipe: that is a normal
/// way to read a few lines, not a reason to abort with a backtrace.
/// The script itself, as text.
///
/// Generated into memory rather than straight into the caller's writer because
/// every shell generator inside `clap_complete` ends its formatting with
/// `write!(...).expect(...)`: a write failure anywhere in the middle becomes a
/// panic before the fallible API ever sees it. Rendering to a `Vec<u8>` cannot
/// fail, so the writing is entirely ours and a closed pipe is just a return
/// value.
#[must_use]
pub fn completion_script(shell: Shell) -> Vec<u8> {
    let mut cmd = command();
    cmd.set_bin_name("niri-tracker");
    let generator: clap_complete::Shell = shell.into();

    let mut buffer = Vec::new();
    clap_complete::generate(generator, &mut cmd, "niri-tracker", &mut buffer);
    buffer
}

/// Write a completion script for `shell` into `out`.
///
/// The program name is passed explicitly rather than taken from the crate name:
/// the two happen to agree today, and a rename should not silently produce a
/// completion script that completes a command nobody has.
///
/// Errors are returned rather than panicking. A completion script is usually
/// piped into `head` or `less`, and a reader that stops early closes the pipe:
/// that is a normal way to read a few lines, not a reason to abort with a
/// backtrace.
pub fn write_completions(shell: Shell, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    out.write_all(&completion_script(shell))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_shell_produces_output() {
        for shell in [Shell::Bash, Shell::Fish, Shell::Zsh] {
            let mut buf = Vec::new();
            write_completions(shell, &mut buf).expect("writes into a Vec");
            let text = String::from_utf8(buf).expect("completions are UTF-8");
            assert!(!text.is_empty(), "{shell:?} produced nothing");
            assert!(
                text.contains("niri-tracker"),
                "{shell:?} does not name the program"
            );
        }
    }

    #[test]
    fn a_closed_pipe_is_an_error_not_a_panic() {
        // A reader that stops early, as `| head` does, closes the pipe. The write
        // must come back as an error: panicking would print a backtrace for a
        // perfectly ordinary way to read the output.
        struct Closed;

        impl std::io::Write for Closed {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let result = write_completions(Shell::Fish, &mut Closed);
        assert_eq!(
            result.expect_err("a closed pipe must be reported").kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn details_requires_an_app() {
        let err = Cli::try_parse_from(["niri-tracker", "details"]);
        assert!(
            err.is_err(),
            "details without --app must be refused by clap"
        );
    }
}
