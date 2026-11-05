//! Turning stored intervals into report rows.
//!
//! Everything a report needs is decided here: how rows are grouped, how aliases
//! rewrite names, how titles are normalised, and how a limit is applied without
//! corrupting the total. The daemon calls this, so a report and `status` are
//! always computed from the same rules.

use chrono::{DateTime, Datelike as _, Local, Timelike as _};

use crate::config::{AliasConfig, Resolved, Unmatched};
use crate::db::Session;
use crate::ipc::{GroupKey, QueryArgs, Report, Row};

/// A stretch of time credited to one name, after every rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attributed {
    /// The name the time is credited to when grouping by application: the
    /// app_id, or the alias that claimed it.
    pub name: String,
    /// The window title, kept separate because grouping by title needs it and
    /// the two must not be confused.
    pub title: String,
    /// When it began, kept so day and hour grouping works.
    pub start: DateTime<chrono::Utc>,
    /// How long it lasted, in whole seconds.
    pub seconds: i64,
}

/// Build a report from stored sessions plus the stretch still being recorded.
///
/// `live` is included so that a report always agrees with `status`: asking for
/// today's total should not depend on whether the current interval happens to be
/// idle yet.
#[must_use]
pub fn build(
    sessions: &[Session],
    live: Option<&crate::db::Live>,
    query: &QueryArgs,
    config: &Resolved,
) -> Report {
    let window = window_of(query);
    let mut attributed: Vec<Attributed> = Vec::with_capacity(sessions.len());

    for session in sessions {
        if query.exclude_passive && session.passive {
            continue;
        }
        let Some((start, end)) = clip(session.start, session.end, window) else {
            continue;
        };
        attribute_into(
            &mut attributed,
            &session.app_id,
            &session.title,
            start,
            end,
            query,
            config,
        );
    }

    if let Some(live) = live
        && !(query.exclude_passive && live.passive)
    {
        let now = chrono::Utc::now();
        let Some((start, end)) = clip(live.start, now, window) else {
            return group(attributed, query);
        };
        attribute_into(
            &mut attributed,
            &live.app_id,
            &live.title,
            start,
            end,
            query,
            config,
        );
    }

    group(attributed, query)
}

/// The half-open instant range covered by the query's inclusive local days.
fn window_of(query: &QueryArgs) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    let from = crate::db::local_midnight(query.from);
    let to = crate::db::local_midnight(query.to.succ_opt().unwrap_or(query.to));
    (from, to)
}

/// Trim a stretch to the query window, or `None` when it falls outside entirely.
///
/// Clipping matters as much as filtering: a stretch that began the day before has
/// to contribute only the part inside the requested range, not all of itself.
fn clip(
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    window: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let (window_start, window_end) = window;
    let start = start.max(window_start);
    let end = end.min(window_end);
    (end > start).then_some((start, end))
}

/// Credit one clipped stretch, split at the boundaries its grouping implies.
fn attribute_into(
    attributed: &mut Vec<Attributed>,
    app_id: &str,
    title: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    query: &QueryArgs,
    config: &Resolved,
) {
    for (chunk_start, chunk_end) in split(query.by, start, end) {
        let seconds = (chunk_end - chunk_start).num_seconds();
        if seconds <= 0 {
            continue;
        }
        let piece = attribute(app_id, title, chunk_start, seconds, config);
        if matches_query(&piece, query) {
            attributed.push(piece);
        }
    }
}

/// Split a stretch at the local day or hour boundaries that `by` groups on.
///
/// Without this a stretch that spans midnight is credited entirely to the day it
/// started, and a two-hour stretch shows up as a single row under the hour it
/// began — both of which make the grouping mean something other than its name.
fn split(
    by: GroupKey,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    if !matches!(by, GroupKey::Day | GroupKey::Hour) {
        return vec![(start, end)];
    }

    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let local = cursor.with_timezone(&Local);
        let boundary = match by {
            GroupKey::Day => {
                let next_day = local.date_naive().succ_opt().unwrap_or(local.date_naive());
                crate::db::local_midnight(next_day)
            }
            GroupKey::Hour => {
                // Truncate to the local hour, then step one hour. Adding to the
                // instant rather than the wall time keeps the chunks contiguous
                // across a daylight-saving change.
                let hour = local
                    .with_minute(0)
                    .and_then(|value| value.with_second(0))
                    .and_then(|value| value.with_nanosecond(0));
                match hour {
                    Some(hour) => (hour + chrono::Duration::hours(1)).with_timezone(&chrono::Utc),
                    None => end,
                }
            }
            GroupKey::App | GroupKey::Title => end,
        };
        let stop = boundary.min(end);
        if stop <= cursor {
            // A boundary that does not advance would loop forever; take the rest.
            chunks.push((cursor, end));
            break;
        }
        chunks.push((cursor, stop));
        cursor = stop;
    }
    chunks
}

/// Apply the alias and normalisation rules to one stretch.
///
/// Produces the credited application name and the report-ready title. An alias
/// that matches a title replaces both: the point of such a rule is that the
/// pieces are reported as distinct entries under the alias's name.
fn attribute(
    app_id: &str,
    title: &str,
    start: DateTime<chrono::Utc>,
    seconds: i64,
    config: &Resolved,
) -> Attributed {
    let report_title = if config.normalize_titles {
        normalise_title(title, config.strip_counters)
    } else {
        title.to_owned()
    };

    for alias in &config.aliases {
        if !alias.app_ids.iter().any(|id| id == app_id) {
            continue;
        }

        // No title rules: the alias simply renames the app, and the title still
        // belongs to that app.
        if alias.titles.is_empty() {
            return Attributed {
                name: alias.name.clone(),
                title: report_title,
                start,
                seconds,
            };
        }

        // Title rules: a match means this stretch is reported as the alias.
        if alias
            .titles
            .iter()
            .any(|needle| title.contains(needle.as_str()))
        {
            return Attributed {
                name: alias.name.clone(),
                title: alias.name.clone(),
                start,
                seconds,
            };
        }

        // Nothing matched. Folding it into the alias would silently
        // reclassify every other title, so by default it keeps its own name.
        let name = match alias.unmatched {
            Unmatched::Keep => app_id.to_owned(),
            Unmatched::Other => format!("{} (Other)", alias.name),
        };
        return Attributed {
            name,
            title: report_title,
            start,
            seconds,
        };
    }

    Attributed {
        name: app_id.to_owned(),
        title: report_title,
        start,
        seconds,
    }
}

/// Whether a stretch passes the filters in `query`.
fn matches_query(piece: &Attributed, query: &QueryArgs) -> bool {
    if let Some(app) = &query.app
        && &piece.name != app
    {
        return false;
    }
    if let Some(title) = &query.title
        && !piece.title.contains(title.as_str())
    {
        return false;
    }
    true
}

/// Collapse attributed stretches into rows by the requested key.
fn group(pieces: Vec<Attributed>, query: &QueryArgs) -> Report {
    let mut totals: Vec<(String, i64)> = Vec::new();

    for piece in pieces {
        let key = match query.by {
            GroupKey::App => piece.name.clone(),
            GroupKey::Title => piece.title.clone(),
            GroupKey::Day => {
                let local = piece.start.with_timezone(&Local);
                format!(
                    "{:04}-{:02}-{:02}",
                    local.year(),
                    local.month(),
                    local.day()
                )
            }
            GroupKey::Hour => {
                let local = piece.start.with_timezone(&Local);
                format!("{:02}:00", local.hour())
            }
        };

        match totals.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, total)) => *total += piece.seconds,
            None => totals.push((key, piece.seconds)),
        }
    }

    // Largest first, then alphabetically, so two rows of equal length do not
    // swap places between runs.
    totals.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    // The total covers everything counted, before a limit hides rows.
    let total_seconds = totals.iter().map(|(_, seconds)| seconds).sum();

    let mut rows: Vec<Row> = totals
        .into_iter()
        .map(|(name, seconds)| Row { name, seconds })
        .collect();

    if let Some(limit) = query.limit {
        rows.truncate(limit);
    }

    Report {
        rows,
        total_seconds,
    }
}

/// Tidy a window title for grouping.
///
/// Deliberately conservative: a trailing counter such as `(3)` or `[12]` is
/// removed and runs of whitespace collapse. Numbers elsewhere are left alone,
/// because deleting every digit turns `2048` into `48` and `Go 1.23` into
/// `Go ..`, which is worse than a little fragmentation.
#[must_use]
pub fn normalise_title(title: &str, strip_counters: bool) -> String {
    let mut text = title.trim().to_owned();

    if strip_counters {
        text = strip_trailing_counter(&text);
    }

    // Collapse whitespace without pulling in a regex engine.
    let mut collapsed = String::with_capacity(text.len());
    let mut previous_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                collapsed.push(' ');
            }
            previous_was_space = true;
        } else {
            collapsed.push(ch);
            previous_was_space = false;
        }
    }

    collapsed.trim_end().to_owned()
}

/// Remove one trailing `(N)`, `[N]` or `- N` marker.
///
/// Only a counter at the very end counts, and only when the brackets hold
/// nothing but digits, so `Page (1 of 3)` and `(draft)` survive intact.
fn strip_trailing_counter(text: &str) -> String {
    let trimmed = text.trim_end();

    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(rest) = trimmed.strip_suffix(close)
            && let Some(position) = rest.rfind(open)
        {
            let inside = &rest[position + 1..];
            if !inside.is_empty() && inside.bytes().all(|byte| byte.is_ascii_digit()) {
                let head = rest[..position].trim_end();
                // A title that is only the counter keeps it, rather than
                // becoming empty.
                if !head.is_empty() {
                    return head.to_owned();
                }
            }
        }
    }

    // A bare trailing " - 3" is common in terminals and editors.
    if let Some((head, tail)) = trimmed.rsplit_once(" - ")
        && !tail.is_empty()
        && tail.bytes().all(|byte| byte.is_ascii_digit())
    {
        let head = head.trim_end();
        if !head.is_empty() {
            return head.to_owned();
        }
    }

    trimmed.to_owned()
}

/// The names an alias list would use, for `--help` and for tests.
#[must_use]
pub fn alias_names(aliases: &[AliasConfig]) -> Vec<&str> {
    aliases.iter().map(|alias| alias.name.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn config() -> Resolved {
        let preset: crate::config::Config =
            toml::from_str(crate::config::PRESET).expect("preset parses");
        preset.resolve().expect("preset resolves")
    }

    fn query(by: GroupKey) -> QueryArgs {
        QueryArgs {
            from: chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            to: chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            by,
            app: None,
            title: None,
            exclude_passive: false,
            limit: None,
        }
    }

    fn session(app_id: &str, title: &str, seconds: i64) -> Session {
        let start = chrono::Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 0).unwrap();
        Session {
            id: 1,
            start,
            end: start + chrono::Duration::seconds(seconds),
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            passive: false,
            closed: true,
        }
    }

    #[test]
    fn rows_are_totalled_and_sorted() {
        let sessions = vec![
            session("kitty", "a", 30),
            session("firefox", "b", 90),
            session("kitty", "c", 60),
        ];
        let report = build(&sessions, None, &query(GroupKey::App), &config());
        assert_eq!(report.total_seconds, 180);
        // Equal totals break the tie alphabetically, so runs are reproducible.
        assert_eq!(
            report.rows[0],
            Row {
                name: String::from("firefox"),
                seconds: 90
            }
        );
        assert_eq!(
            report.rows[1],
            Row {
                name: String::from("kitty"),
                seconds: 90
            }
        );
    }

    #[test]
    fn the_limit_hides_rows_but_not_the_total() {
        let sessions = vec![
            session("a", "", 30),
            session("b", "", 60),
            session("c", "", 90),
        ];
        let mut args = query(GroupKey::App);
        args.limit = Some(1);
        let report = build(&sessions, None, &args, &config());
        assert_eq!(report.rows.len(), 1);
        assert_eq!(
            report.total_seconds, 180,
            "the total must cover hidden rows"
        );
    }

    #[test]
    fn passive_time_can_be_left_out() {
        let mut passive = session("video", "", 60);
        passive.passive = true;
        let sessions = vec![session("kitty", "", 30), passive];
        let mut args = query(GroupKey::App);
        args.exclude_passive = true;
        let report = build(&sessions, None, &args, &config());
        assert_eq!(report.total_seconds, 30);
    }

    #[test]
    fn the_app_filter_matches_the_credited_name() {
        let sessions = vec![session("kitty", "", 30), session("firefox", "", 60)];
        let mut args = query(GroupKey::App);
        args.app = Some(String::from("kitty"));
        let report = build(&sessions, None, &args, &config());
        assert_eq!(report.total_seconds, 30);
    }

    #[test]
    fn the_title_filter_is_a_substring_match() {
        let sessions = vec![
            session("a", "GitHub - Pull request", 30),
            session("a", "Inbox", 60),
        ];
        let mut args = query(GroupKey::App);
        args.title = Some(String::from("GitHub"));
        let report = build(&sessions, None, &args, &config());
        assert_eq!(report.total_seconds, 30);
    }

    #[test]
    fn a_trailing_counter_is_stripped() {
        assert_eq!(normalise_title("Inbox (3)", true), "Inbox");
        assert_eq!(normalise_title("Tab  [12]", true), "Tab");
        assert_eq!(normalise_title("file.txt - 4", true), "file.txt");
    }

    #[test]
    fn counters_elsewhere_are_left_alone() {
        assert_eq!(normalise_title("2048", true), "2048");
        assert_eq!(normalise_title("Go 1.23 release", true), "Go 1.23 release");
        assert_eq!(normalise_title("Page (1 of 3)", true), "Page (1 of 3)");
        assert_eq!(normalise_title("(draft)", true), "(draft)");
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(normalise_title("a   b \t c", false), "a b c");
    }

    #[test]
    fn a_counter_alone_is_kept() {
        assert_eq!(normalise_title("(3)", true), "(3)");
    }

    #[test]
    fn grouping_by_day_uses_the_local_date() {
        let mut sessions = vec![session("a", "", 60)];
        sessions[0].start = chrono::Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();
        sessions[0].end = sessions[0].start + chrono::Duration::seconds(60);

        // The stretch has to be asked for by its own day: a report for the 5th must
        // not include the 4th.
        let mut args = query(GroupKey::Day);
        args.from = chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        args.to = args.from;
        let report = build(&sessions, None, &args, &config());
        assert_eq!(report.rows[0].name, "2026-11-04");

        let outside = build(&sessions, None, &query(GroupKey::Day), &config());
        assert_eq!(
            outside.total_seconds, 0,
            "the 4th must not appear in the 5th"
        );
    }

    #[test]
    fn grouping_by_hour_uses_the_local_hour() {
        let sessions = vec![session("a", "", 60)];
        let report = build(&sessions, None, &query(GroupKey::Hour), &config());
        let expected = format!("{:02}:00", 10);
        let local_hour = format!("{:02}:00", sessions[0].start.with_timezone(&Local).hour());
        assert!(
            report.rows[0].name == local_hour || report.rows[0].name == expected,
            "got {}",
            report.rows[0].name
        );
    }

    #[test]
    fn the_live_stretch_only_counts_inside_the_window() {
        let now = chrono::Utc::now();
        let live = crate::db::Live {
            app_id: String::from("kitty"),
            title: String::new(),
            passive: false,
            start: now - chrono::Duration::seconds(100),
        };

        let mut args = query(GroupKey::App);
        args.from = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        args.to = args.from;
        let report = build(&[], Some(&live), &args, &config());
        assert_eq!(
            report.total_seconds, 0,
            "time being recorded now cannot belong to a day in 2020"
        );

        let mut today = query(GroupKey::App);
        today.from = now.with_timezone(&Local).date_naive();
        today.to = today.from;
        let report = build(&[], Some(&live), &today, &config());
        assert!(report.total_seconds > 0, "today's live stretch must count");
    }

    #[test]
    fn a_stretch_crossing_midnight_is_split_between_the_days() {
        let midnight =
            crate::db::local_midnight(chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap());
        let start = midnight - chrono::Duration::minutes(30);
        let session = Session {
            id: 1,
            start,
            end: midnight + chrono::Duration::minutes(30),
            app_id: String::from("kitty"),
            title: String::new(),
            passive: false,
            closed: true,
        };

        let mut args = query(GroupKey::Day);
        args.from = chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        args.to = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let report = build(&[session], None, &args, &config());

        assert_eq!(report.total_seconds, 3600, "the whole hour is counted");
        let by_day: std::collections::HashMap<&str, i64> = report
            .rows
            .iter()
            .map(|row| (row.name.as_str(), row.seconds))
            .collect();
        assert_eq!(
            by_day.get("2026-11-05"),
            Some(&1800),
            "half before midnight"
        );
        assert_eq!(by_day.get("2026-11-06"), Some(&1800), "half after");
    }

    #[test]
    fn a_stretch_crossing_an_hour_is_split_between_the_hours() {
        // Built from a local midnight so the hours are the same in any timezone:
        // 10:30 to 12:30 covers a half hour, a whole hour, and another half hour.
        let midnight =
            crate::db::local_midnight(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap());
        let base = midnight + chrono::Duration::minutes(630);
        let session = Session {
            id: 1,
            start: base,
            end: base + chrono::Duration::minutes(120),
            app_id: String::from("kitty"),
            title: String::new(),
            passive: false,
            closed: true,
        };

        let report = build(&[session], None, &query(GroupKey::Hour), &config());
        assert_eq!(report.total_seconds, 7200);
        let seconds: Vec<i64> = report.rows.iter().map(|row| row.seconds).collect();
        assert_eq!(seconds, vec![3600, 1800, 1800], "rows: {:?}", report.rows);
    }

    #[test]
    fn a_stretch_is_clipped_to_a_partial_day() {
        let midnight =
            crate::db::local_midnight(chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap());
        // Runs from 30 minutes before midnight to 30 after.
        let session = Session {
            id: 1,
            start: midnight - chrono::Duration::minutes(30),
            end: midnight + chrono::Duration::minutes(30),
            app_id: String::from("kitty"),
            title: String::new(),
            passive: false,
            closed: true,
        };

        let mut args = query(GroupKey::App);
        args.from = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        args.to = args.from;
        let report = build(&[session], None, &args, &config());
        assert_eq!(
            report.total_seconds, 1800,
            "only the part inside the requested day counts"
        );
    }

    #[test]
    fn an_alias_renames_a_listed_app() {
        let mut cfg = config();
        cfg.aliases = vec![AliasConfig {
            name: String::from("Browser"),
            app_ids: vec![String::from("floorp")],
            titles: Vec::new(),
            unmatched: Unmatched::Keep,
        }];
        let sessions = vec![session("floorp", "anything", 60)];
        let report = build(&sessions, None, &query(GroupKey::App), &cfg);
        assert_eq!(report.rows[0].name, "Browser");
    }

    #[test]
    fn an_alias_with_titles_keeps_unmatched_titles_by_default() {
        let mut cfg = config();
        cfg.aliases = vec![AliasConfig {
            name: String::from("Browser"),
            app_ids: vec![String::from("floorp")],
            titles: vec![String::from("GitHub")],
            unmatched: Unmatched::Keep,
        }];
        let sessions = vec![
            session("floorp", "GitHub - x", 60),
            session("floorp", "Example", 30),
        ];
        let report = build(&sessions, None, &query(GroupKey::App), &cfg);
        let names: Vec<&str> = report.rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"Browser"), "got {names:?}");
        assert!(
            names.contains(&"floorp"),
            "the unmatched title must keep its own name; got {names:?}"
        );
    }

    #[test]
    fn an_alias_can_fold_unmatched_titles_instead() {
        let mut cfg = config();
        cfg.aliases = vec![AliasConfig {
            name: String::from("Browser"),
            app_ids: vec![String::from("floorp")],
            titles: vec![String::from("GitHub")],
            unmatched: Unmatched::Other,
        }];
        let sessions = vec![session("floorp", "Example", 30)];
        let report = build(&sessions, None, &query(GroupKey::App), &cfg);
        assert_eq!(report.rows[0].name, "Browser (Other)");
    }
}
