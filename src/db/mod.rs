//! SQLite storage.
//!
//! One table, one writer. The daemon owns every write; readers (reports, run
//! through the daemon) see a consistent picture because the daemon answers from
//! the same connection it writes with.
//!
//! Timestamps are stored as Unix seconds in UTC and as text in the row's local
//! date, which keeps range queries integer comparisons while letting a day
//! boundary be exact without timezone arithmetic at read time.

mod session;

pub use session::{Live, Session, local_midnight};

use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension as _, params};

/// Why the database refused a request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be opened, or a statement failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The directory holding the database could not be created.
    #[error("could not create {path}: {source}", path = .path.display())]
    Directory {
        /// The directory that was refused.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A handle to the database file.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Directory {
                path: parent.to_owned(),
                source,
            })?;
        }

        let conn = Connection::open(path)?;

        // WAL lets a read proceed while the daemon writes, and survives a crash
        // without needing the journal replayed by hand.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Durability is not worth an fsync per interval: losing the last few
        // seconds of screen time after a power cut is an acceptable trade.
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let store = Self { conn };
        store.create_schema()?;
        Ok(store)
    }

    /// Open an in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, Error> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.create_schema()?;
        Ok(store)
    }

    fn create_schema(&self) -> Result<(), Error> {
        self.conn.execute_batch(
            r#"
			CREATE TABLE IF NOT EXISTS session (
				id      INTEGER PRIMARY KEY AUTOINCREMENT,
				start   INTEGER NOT NULL,
				end     INTEGER NOT NULL,
				day     TEXT    NOT NULL,
				app_id  TEXT    NOT NULL,
				title   TEXT    NOT NULL,
				passive INTEGER NOT NULL DEFAULT 0,
				closed  INTEGER NOT NULL DEFAULT 0
			);

			CREATE INDEX IF NOT EXISTS session_day ON session (day, app_id);
			CREATE INDEX IF NOT EXISTS session_start ON session (start);
			"#,
        )?;
        Ok(())
    }

    /// Begin a stretch, returning its row id.
    ///
    /// The row starts out open with `end == start`; [`Self::extend`] moves the
    /// end forward as time passes, and [`Self::close`] marks it finished.
    pub fn begin(&self, live: &Live) -> Result<i64, Error> {
        let day = live
            .start
            .with_timezone(&chrono::Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        self.conn.execute(
            "INSERT INTO session (start, end, day, app_id, title, passive, closed) \
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                live.start.timestamp(),
                live.start.timestamp(),
                day,
                live.app_id,
                live.title,
                i64::from(live.passive),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Move an open row's end forward.
    pub fn extend(&self, id: i64, end: DateTime<Utc>) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE session SET end = ?1 WHERE id = ?2 AND closed = 0",
            params![end.timestamp(), id],
        )?;
        Ok(())
    }

    /// Mark a row finished at `end`.
    pub fn close(&self, id: i64, end: DateTime<Utc>) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE session SET end = ?1, closed = 1 WHERE id = ?2",
            params![end.timestamp(), id],
        )?;
        Ok(())
    }

    /// Rewrite a row's start, used when a stretch is cut back to the moment
    /// input actually stopped.
    pub fn set_start(&self, id: i64, start: DateTime<Utc>) -> Result<(), Error> {
        let day = start
            .with_timezone(&chrono::Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        self.conn.execute(
            "UPDATE session SET start = ?1, day = ?2 WHERE id = ?3",
            params![start.timestamp(), day, id],
        )?;
        Ok(())
    }

    /// Close every row left open by a previous run.
    ///
    /// A row that is open when the daemon starts belongs to a process that died
    /// without finishing it. Its end is already the last checkpoint, so closing
    /// it is enough; leaving it open would make it look like time still being
    /// accumulated.
    ///
    /// Returns how many rows were closed.
    pub fn close_dangling(&self) -> Result<usize, Error> {
        let changed = self
            .conn
            .execute("UPDATE session SET closed = 1 WHERE closed = 0", [])?;
        Ok(changed)
    }

    /// Read every finished stretch whose local day falls in the inclusive range.
    pub fn range(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<Session>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, start, end, app_id, title, passive, closed \
			 FROM session WHERE day BETWEEN ?1 AND ?2 ORDER BY start",
        )?;

        let rows = stmt.query_map(
            params![
                from.format("%Y-%m-%d").to_string(),
                to.format("%Y-%m-%d").to_string()
            ],
            |row| {
                let start_secs: i64 = row.get(1)?;
                let end_secs: i64 = row.get(2)?;
                Ok(Session {
                    id: row.get(0)?,
                    start: DateTime::from_timestamp(start_secs, 0).unwrap_or_default(),
                    end: DateTime::from_timestamp(end_secs, 0).unwrap_or_default(),
                    app_id: row.get(3)?,
                    title: row.get(4)?,
                    passive: row.get::<_, i64>(5)? != 0,
                    closed: row.get::<_, i64>(6)? != 0,
                })
            },
        )?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    /// Delete finished stretches whose local day is before `before`.
    ///
    /// Returns how many rows went away.
    pub fn delete_before(&self, before: NaiveDate) -> Result<usize, Error> {
        let changed = self.conn.execute(
            "DELETE FROM session WHERE day < ?1 AND closed = 1",
            params![before.format("%Y-%m-%d").to_string()],
        )?;
        Ok(changed)
    }

    /// The oldest local day present, if any.
    pub fn oldest_day(&self) -> Result<Option<NaiveDate>, Error> {
        let stored = self
            .conn
            .query_row("SELECT MIN(day) FROM session", [], |row| {
                row.get::<_, Option<String>>(0)
            })?;
        // A stored day is written by this program in this exact format, so an
        // unparseable one means the file was edited by hand; treat that as "no
        // day to speak of" rather than failing the whole run.
        Ok(stored.and_then(|day| NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok()))
    }

    /// How many intervals are stored, for `status`.
    pub fn count(&self) -> Result<u64, Error> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))?;
        Ok(count.max(0).unsigned_abs())
    }

    /// Whether a row is still open. Used by tests and by `status`.
    pub fn is_open(&self, id: i64) -> Result<Option<bool>, Error> {
        let value = self
            .conn
            .query_row(
                "SELECT closed FROM session WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(value.map(|closed| closed == 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn live(app_id: &str, start: DateTime<Utc>) -> Live {
        Live {
            app_id: app_id.to_owned(),
            title: String::from("a title"),
            passive: false,
            start,
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    #[test]
    fn a_stretch_round_trips() {
        let store = store();
        let start = Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 0).unwrap();
        let id = store.begin(&live("kitty", start)).unwrap();
        store
            .close(id, start + chrono::Duration::minutes(5))
            .unwrap();

        let day = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let sessions = store.range(day, day).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].app_id, "kitty");
        assert_eq!(sessions[0].duration(), chrono::Duration::minutes(5));
        assert!(sessions[0].closed);
    }

    #[test]
    fn dangling_rows_are_closed_on_start() {
        let store = store();
        let start = at(1_700_000_000);
        let id = store.begin(&live("kitty", start)).unwrap();
        store
            .extend(id, start + chrono::Duration::seconds(30))
            .unwrap();
        assert_eq!(store.is_open(id).unwrap(), Some(true));

        let closed = store.close_dangling().unwrap();
        assert_eq!(closed, 1);
        assert_eq!(store.is_open(id).unwrap(), Some(false));

        // Its recorded end is the last checkpoint, not the restart time.
        let day = start.with_timezone(&chrono::Local).date_naive();
        let sessions = store.range(day, day).unwrap();
        assert_eq!(sessions[0].duration(), chrono::Duration::seconds(30));
    }

    #[test]
    fn the_range_is_inclusive_and_bounded_by_local_day() {
        let store = store();
        let day_one = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();
        let day_two = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        for (app, start) in [("a", day_one), ("b", day_two)] {
            let id = store.begin(&live(app, start)).unwrap();
            store
                .close(id, start + chrono::Duration::minutes(1))
                .unwrap();
        }

        let first = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let second = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        assert_eq!(store.range(first, first).unwrap().len(), 1);
        assert_eq!(store.range(second, second).unwrap().len(), 1);
        assert_eq!(store.range(first, second).unwrap().len(), 2);
    }

    #[test]
    fn old_rows_can_be_deleted() {
        let store = store();
        let old = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let id = store.begin(&live("a", old)).unwrap();
        store.close(id, old + chrono::Duration::minutes(1)).unwrap();

        let cutoff = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        assert_eq!(store.delete_before(cutoff).unwrap(), 1);
        assert_eq!(store.oldest_day().unwrap(), None);
    }
}
