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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    let config_path = match cli.config.clone() {
        Some(path) => path,
        None => config::config_file()?,
    };

    match cli.command {
        Command::Daemon => {
            // Before anything can complain, so a configuration error is reported
            // through the same formatting as everything else.
            niri_tracker::daemon::init_logging();
            let resolved = config::load_from(&config_path)?;
            niri_tracker::daemon::run(&resolved, &config_path)?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Report(args) => report(args),
        Command::Details(args) => details(args),

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
///
/// The configuration is not read here. Aliases and title normalisation are applied
/// by the daemon, which is the only place that knows them, so loading the file on
/// this side would parse it and throw the result away.
fn report(args: ReportArgs) -> Result<ExitCode, BoxError> {
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
fn details(args: DetailsArgs) -> Result<ExitCode, BoxError> {
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

/// Fill in defaults for a query over inclusive local-day bounds.
///
/// Both `from` and `to` are inclusive dates (not instants); the daemon
/// compares local days, so a range is inclusive by construction and there
/// is no off-by-one to express. A single-sided range is allowed: omitted
/// `to` means "through today", omitted `from` means "the `to` day".
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
    let to = to.unwrap_or(today);
    let from = from.unwrap_or(to);
    debug_assert!(from <= to, "from {from} is after to {to}");
    QueryArgs {
        from,
        to,
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
///
/// Columns are measured in terminal cells, not characters: a Chinese title takes
/// two cells per character, so counting characters would print a column that looks
/// ragged to anyone reading it.
fn print_report(report: &Report) {
    if report.rows.is_empty() {
        // Nothing to list, but the period may still have time in it: `--limit 0`
        // hides every row without hiding the total.
        if report.total_seconds > 0 {
            println!(
                "{}",
                config::format_duration(std::time::Duration::from_secs(
                    report.total_seconds.unsigned_abs()
                )),
            );
        } else {
            println!("(nothing recorded for this period)");
        }
        return;
    }

    let width = report
        .rows
        .iter()
        .map(|row| display_width(&row.name))
        .max()
        .unwrap_or(0)
        .clamp(8, 80);

    for row in &report.rows {
        println!(
            "{}  {}",
            pad(&truncate(&row.name, width), width),
            config::format_duration(std::time::Duration::from_secs(row.seconds.unsigned_abs())),
        );
    }

    println!();
    println!(
        "{}  {}",
        pad("Total", width),
        config::format_duration(std::time::Duration::from_secs(
            report.total_seconds.unsigned_abs()
        )),
    );
}

/// How many terminal cells `text` occupies.
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Pad `text` with spaces to `width` cells.
///
/// `format!`'s `{:<width$}` pads by characters, which is the same thing only for
/// text that is entirely one cell per character.
fn pad(text: &str, width: usize) -> String {
    let used = display_width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

/// Shorten a label to `width` cells, marking the cut.
///
/// The cut is by cells, and it never splits a character from its combining marks:
/// those are zero cells wide, so they are included with the character before them
/// rather than being cut away from it.
fn truncate(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }

    // One cell is kept for the ellipsis.
    let budget = width.saturating_sub(1);
    let mut shortened = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + cells > budget {
            break;
        }
        shortened.push(character);
        used += cells;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_is_its_length() {
        assert_eq!(display_width("nvim"), 4);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn cjk_is_two_cells_per_character() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("理解项目"), 8);
    }

    #[test]
    fn mixed_text_adds_up() {
        // Two Chinese characters plus " ab" plus two more.
        assert_eq!(display_width("中文 ab 中文"), 4 + 1 + 2 + 1 + 4);
    }

    #[test]
    fn a_combining_mark_adds_no_width() {
        // "e" followed by U+0301 COMBINING ACUTE ACCENT renders as one cell.
        assert_eq!(display_width("e\u{301}"), 1);
    }

    #[test]
    fn padding_counts_cells_not_characters() {
        // Four cells wide, so two spaces, not four.
        assert_eq!(pad("中文", 6), "中文  ");
        assert_eq!(display_width(&pad("中文", 6)), 6);
    }

    #[test]
    fn padding_never_shrinks_a_long_string() {
        assert_eq!(pad("abcdefgh", 3), "abcdefgh");
    }

    #[test]
    fn truncation_counts_cells_not_characters() {
        // Twelve cells of Chinese, cut to six. Each character is two cells, so at
        // most two fit before the ellipsis; one cell is left for `pad` to fill,
        // which is what keeps the column aligned.
        let cut = truncate("中文中文中文", 6);
        assert!(cut.ends_with('…'), "got {cut:?}");
        assert_eq!(cut, "中文…");
        assert!(display_width(&cut) <= 6);
    }

    #[test]
    fn truncation_leaves_fitting_text_alone() {
        assert_eq!(truncate("nvim", 8), "nvim");
        assert_eq!(truncate("中文", 4), "中文");
    }

    #[test]
    fn truncation_does_not_split_a_character_from_its_combining_mark() {
        // Four cells cut to three, which leaves a budget of two: "x" and "e" fit,
        // the accent is free, and "y" does not. The accent must go with its base
        // rather than being cut away from it.
        let cut = truncate("xe\u{301}yy", 3);
        assert_eq!(
            cut, "xe\u{301}…",
            "a combining mark must stay with its base"
        );
        assert_eq!(display_width(&cut), 3);
    }

    #[test]
    fn truncation_never_exceeds_the_width() {
        for width in 2..=20 {
            for text in ["中文中文中文中文", "mixed 中文 text", "abcdefghij"] {
                let cut = truncate(text, width);
                assert!(
                    display_width(&cut) <= width,
                    "{text:?} cut to {width} is {} cells: {cut:?}",
                    display_width(&cut)
                );
            }
        }
    }

    #[test]
    fn every_truncated_row_pads_to_the_same_column() {
        // This is the bug that prompted the fix: rows of Chinese titles used to
        // push the duration column to a different place on every line.
        let names = [
            "理解项目，只看源码就行",
            "fish",
            "Yazi: niri-tracker",
            "图片查看器",
        ];
        let width = names
            .iter()
            .map(|name| display_width(name))
            .max()
            .unwrap_or(0);
        let columns: Vec<usize> = names
            .iter()
            .map(|name| display_width(&pad(&truncate(name, width), width)))
            .collect();
        assert!(
            columns.iter().all(|column| *column == width),
            "rows ended at different columns: {columns:?}"
        );
    }
}
