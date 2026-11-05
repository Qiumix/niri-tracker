//! Human-readable durations.
//!
//! The configuration spells a duration as `"90s"`, `"5m"` or `"1h30m"`, and a
//! bare number is read as seconds. Parsing is strict: anything else is an error
//! rather than a silent zero, because a mistyped timeout would otherwise look
//! like "idle detection is on" while it is not.

use std::time::Duration;

/// Why a duration string was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DurationError {
    /// The string was empty or all whitespace.
    #[error("expected a duration such as \"5m\", found nothing")]
    Empty,
    /// The string did not follow the `<number><unit>` grammar.
    #[error("expected a duration such as \"5m\" or \"1h30m\", found {0:?}")]
    Invalid(String),
    /// The value overflowed the range a `Duration` can hold.
    #[error("the duration {0:?} is too large")]
    TooLarge(String),
}

/// Parse a duration such as `"0s"`, `"90s"`, `"5m"`, `"1h30m"` or `"5"`.
///
/// A bare number means seconds. Units are `s`, `m`, `h` and `d`, and may be
/// combined as long as each unit appears at most once and in descending order of
/// magnitude.
pub fn parse(input: &str) -> Result<Duration, DurationError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(DurationError::Empty);
    }

    // A bare number is seconds, which is what a hand-written config usually means.
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        let secs = text
            .parse::<u64>()
            .map_err(|_| DurationError::TooLarge(text.to_owned()))?;
        return Ok(Duration::from_secs(secs));
    }

    let mut total = Duration::ZERO;
    let mut pending: Option<u64> = None;
    let mut previous_unit = u64::MAX;

    for ch in text.chars() {
        if let Some(digit) = ch.to_digit(10) {
            let digit = u64::from(digit);
            let value = pending
                .unwrap_or(0)
                .checked_mul(10)
                .and_then(|value| value.checked_add(digit))
                .ok_or_else(|| DurationError::TooLarge(text.to_owned()))?;
            pending = Some(value);
            continue;
        }

        let value = pending
            .take()
            .ok_or_else(|| DurationError::Invalid(text.to_owned()))?;
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 60 * 60 * 24,
            _ => return Err(DurationError::Invalid(text.to_owned())),
        };

        // "1m1h" is a typo, not a shorthand.
        if unit >= previous_unit {
            return Err(DurationError::Invalid(text.to_owned()));
        }
        previous_unit = unit;

        let secs = value
            .checked_mul(unit)
            .ok_or_else(|| DurationError::TooLarge(text.to_owned()))?;
        total = total
            .checked_add(Duration::from_secs(secs))
            .ok_or_else(|| DurationError::TooLarge(text.to_owned()))?;
    }

    // A trailing number with no unit, as in "5m30".
    if pending.is_some() {
        return Err(DurationError::Invalid(text.to_owned()));
    }

    Ok(total)
}

/// Render a duration for humans, as `"1h 30m"`, `"45s"` or `"0s"`.
///
/// Seconds are omitted once the duration reaches a day, because "2d 3h 4m 5s"
/// reads worse than "2d 3h 4m".
#[must_use]
pub fn format(duration: Duration) -> String {
    let total = duration.as_secs();
    if total == 0 {
        return String::from("0s");
    }

    let days = total / 86_400;
    let hours = total % 86_400 / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;

    let mut parts: Vec<String> = Vec::with_capacity(4);
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 && days == 0 {
        parts.push(format!("{seconds}s"));
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_seconds() {
        assert_eq!(parse("90"), Ok(Duration::from_secs(90)));
    }

    #[test]
    fn single_units() {
        assert_eq!(parse("0s"), Ok(Duration::ZERO));
        assert_eq!(parse("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse("2h"), Ok(Duration::from_secs(7_200)));
        assert_eq!(parse("1d"), Ok(Duration::from_secs(86_400)));
    }

    #[test]
    fn combined_units() {
        assert_eq!(parse("1h30m"), Ok(Duration::from_secs(5_400)));
        assert_eq!(
            parse("1d2h3m4s"),
            Ok(Duration::from_secs(86_400 + 7_200 + 180 + 4))
        );
    }

    #[test]
    fn surrounding_space_is_ignored() {
        assert_eq!(parse("  5m  "), Ok(Duration::from_secs(300)));
    }

    #[test]
    fn rejects_nonsense() {
        assert_eq!(parse(""), Err(DurationError::Empty));
        assert_eq!(parse("   "), Err(DurationError::Empty));
        assert_eq!(parse("m"), Err(DurationError::Invalid("m".into())));
        assert_eq!(parse("5x"), Err(DurationError::Invalid("5x".into())));
        assert_eq!(parse("5m30"), Err(DurationError::Invalid("5m30".into())));
        assert_eq!(parse("1m1h"), Err(DurationError::Invalid("1m1h".into())));
        assert_eq!(parse("-5m"), Err(DurationError::Invalid("-5m".into())));
    }

    #[test]
    fn formats_for_humans() {
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_secs(45)), "45s");
        assert_eq!(format(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format(Duration::from_secs(5_400)), "1h 30m");
        assert_eq!(format(Duration::from_secs(90_000)), "1d 1h");
    }
}
