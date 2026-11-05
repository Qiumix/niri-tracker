//! The stored interval.
//!
//! Time is recorded as intervals rather than samples: one row covers a stretch
//! of time that was spent on one window, which is what a report wants and what
//! keeps the file small. Every row is confined to a single local day, so
//! grouping by day needs no arithmetic over overlapping ranges.

use chrono::{DateTime, Local, NaiveDate, TimeZone as _, Utc};

/// One continuous stretch of time credited to a single window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Row id, used to extend or close the row.
    pub id: i64,
    /// When the stretch began.
    pub start: DateTime<Utc>,
    /// When it ended, as of the last write.
    pub end: DateTime<Utc>,
    /// The application, as the compositor reported it.
    pub app_id: String,
    /// The window title, as the compositor reported it.
    pub title: String,
    /// Whether an idle inhibitor covered this stretch, such as a playing video.
    pub passive: bool,
    /// Whether the row is finished. A run of the daemon that ends abruptly
    /// leaves rows open, which the next start closes rather than counting as
    /// unbounded time.
    pub closed: bool,
}

impl Session {
    /// How long the stretch lasted.
    ///
    /// Never negative, even if a clock adjustment left `end` behind `start`.
    #[must_use]
    pub fn duration(&self) -> chrono::Duration {
        let span = self.end - self.start;
        if span < chrono::Duration::zero() {
            chrono::Duration::zero()
        } else {
            span
        }
    }

    /// The local date the stretch *began* on.
    ///
    /// A stretch may well cross midnight, so this is the start's day and nothing
    /// more. Reports do not use it to decide which day a stretch belongs to; they
    /// clip and split by instant, which is why a stretch crossing midnight appears
    /// in both days with only the part that falls inside each.
    #[must_use]
    pub fn local_date(&self) -> NaiveDate {
        self.start.with_timezone(&Local).date_naive()
    }
}

/// A stretch that is still being recorded, held in memory rather than written.
///
/// The daemon keeps the live stretch here so that answering a report does not
/// need a database write. The database row for it exists but stays unfinished
/// (`closed = false`), and queries ignore it in favour of this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    /// The application being timed.
    pub app_id: String,
    /// Its window title.
    pub title: String,
    /// Whether an idle inhibitor covers it.
    pub passive: bool,
    /// When it began.
    pub start: DateTime<Utc>,
}

impl Live {
    /// How long the stretch has run as of `now`.
    #[must_use]
    pub fn elapsed(&self, now: DateTime<Utc>) -> chrono::Duration {
        let span = now - self.start;
        if span < chrono::Duration::zero() {
            chrono::Duration::zero()
        } else {
            span
        }
    }
}

/// The first instant of `date` in local time.
///
/// Used to cut a stretch at midnight, and to turn the inclusive `--to` date into
/// the exclusive upper bound a range query wants.
#[must_use]
pub fn local_midnight(date: NaiveDate) -> DateTime<Utc> {
    // A local day can have no midnight when a timezone skips it, which
    // `LocalResult::None` reports. Falling back to the UTC interpretation keeps
    // the boundary sane instead of panicking on a holiday.
    match Local.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default()) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
        chrono::LocalResult::None => {
            Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    #[test]
    fn duration_is_never_negative() {
        let session = Session {
            id: 1,
            start: at(100),
            end: at(50),
            app_id: String::from("x"),
            title: String::new(),
            passive: false,
            closed: true,
        };
        assert_eq!(session.duration(), chrono::Duration::zero());
    }

    #[test]
    fn elapsed_is_never_negative() {
        let live = Live {
            app_id: String::from("x"),
            title: String::new(),
            passive: false,
            start: at(100),
        };
        assert_eq!(live.elapsed(at(50)), chrono::Duration::zero());
        assert_eq!(live.elapsed(at(160)), chrono::Duration::seconds(60));
    }

    #[test]
    fn midnight_is_the_start_of_a_local_day() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 5).expect("valid date");
        let midnight = local_midnight(date);
        assert_eq!(midnight.with_timezone(&Local).date_naive(), date);
        assert_eq!(
            midnight.with_timezone(&Local).time(),
            chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid time")
        );
    }
}
