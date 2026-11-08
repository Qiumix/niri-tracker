//! The daemon: one loop that watches the compositor, records intervals, and
//! answers clients.
//!
//! Everything runs on one thread. The only source that blocks is niri's event
//! stream, which gets a thread of its own and forwards what it reads through a
//! channel; the rest — the Wayland connection and the control socket — are polled
//! without blocking, and the main loop wakes every 50 ms to look.
//!
//! That is twenty wake-ups a second, which sounds worse than it is: each one is a
//! non-blocking read and a non-blocking `accept`, and the interval is far longer
//! than the time the processor spends idle, so it is not what keeps a laptop out
//! of its deep states. Waiting on the descriptors with `poll` instead would remove
//! the wake-ups, and would also mean a file descriptor, a self-pipe for the reader
//! thread and a timer to coordinate. The events here arrive a few times a minute,
//! so the trade has not been worth making.
//!
//! The loop owns three things and nothing else does:
//!
//! - the [`Tracker`], which decides what counts;
//! - the [`Store`], which is the only writer of the database;
//! - the [`Server`], which is the only way a client reaches them.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use chrono::Utc;

use crate::compositor::idle::{Idle, IdleEvent};
use crate::compositor::niri::{self, Watch};
use crate::config::Resolved;
use crate::db::Store;
use crate::ipc::{
    Empty, ErrorKind, GroupKey, LiveStatus, Payload, QueryArgs, Report, Request, Response, Server,
    Status, Verb, WireError,
};
use crate::report;
use crate::tracker::{Effect, SystemClock, Tracker};

/// How long to wait before retrying a lost compositor connection.
const RECONNECT_BASE: Duration = Duration::from_secs(1);

/// The longest the reconnect delay grows to.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Why the daemon could not run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database could not be opened.
    #[error("cannot open the database: {0}")]
    Database(#[from] crate::db::Error),

    /// The control socket could not be bound.
    #[error("cannot bind {path}: {source}", path = .path.display())]
    Bind {
        /// The socket path.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// Another daemon already owns this Wayland session.
    #[error("{}", describe_holder(*pid))]
    AlreadyRunning {
        /// The pid the holder recorded, when it could be read.
        pid: Option<u32>,
    },

    /// The data directory itself is unknown.
    #[error(transparent)]
    Paths(#[from] crate::config::PathsError),

    /// The session lock could not be opened.
    #[error("cannot take the session lock in {path}: {source}", path = .path.display())]
    Lock {
        /// The runtime directory holding the lock file.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// Publishing the instance metadata failed.
    #[error("cannot write {path}: {source}", path = .path.display())]
    Instance {
        /// The file that could not be written.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
}

/// Run the daemon until asked to stop.
pub fn run(config: &Resolved, config_path: &Path) -> Result<(), Error> {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| String::from("wayland-0"));
    let runtime_dir = crate::config::runtime_dir();

    // Leftovers from daemons that are gone are safe to remove now that they are
    // known not to be listening. A live one is left alone: its files are about to
    // make the lock below fail, and deleting them would break its clients.
    for found in crate::ipc::discover(&runtime_dir, &display) {
        if !found.alive {
            crate::ipc::InstancePaths::for_pid(&runtime_dir, found.info.pid).cleanup();
        }
    }

    // One daemon per session, decided atomically. Two of them would record every
    // second twice, and nothing about the resulting report would look wrong.
    // Looking for a live instance first would let two daemons that start at the
    // same moment both find none.
    let _lock = match crate::ipc::Lock::acquire(&runtime_dir, &display) {
        Ok(lock) => lock,
        Err(crate::ipc::LockError::Held { pid }) => return Err(Error::AlreadyRunning { pid }),
        Err(crate::ipc::LockError::Io(source)) => {
            return Err(Error::Lock {
                path: runtime_dir.clone(),
                source,
            });
        }
    };

    let database = crate::config::database_file().map_err(Error::Paths)?;
    let store = Store::open(&database)?;
    // Rows left open belong to a process that is gone. Their end is the last
    // checkpoint, so finishing them is all that is needed.
    let dangling = store.close_dangling()?;

    // Applied at startup rather than on a timer: a daemon that runs for weeks
    // should still honour the configured retention, and doing it once per start is
    // enough for a setting measured in months.
    if !config.retention.is_zero() {
        let keep = chrono::Duration::from_std(config.retention).unwrap_or_default();
        let cutoff = (chrono::Local::now() - keep).date_naive();
        match store.delete_before(cutoff) {
            Ok(0) => {}
            Ok(removed) => log_info(&format!("removed {removed} intervals older than {cutoff}")),
            Err(err) => log_warn(&format!("cannot apply retention: {err}")),
        }
    }

    let paths = crate::ipc::InstancePaths::for_pid(&runtime_dir, std::process::id());

    // This instance's files must go away however the daemon ends, including a panic.
    // Registered before the socket is bound so a failure to publish the metadata
    // does not leave a socket behind, and scoped to this instance's own paths: the
    // runtime directory is shared with every other session on the machine.
    struct Cleanup(crate::ipc::InstancePaths);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.cleanup();
        }
    }
    let _cleanup = Cleanup(paths.clone());

    let server = Server::bind(&paths.socket).map_err(|source| Error::Bind {
        path: paths.socket.clone(),
        source,
    })?;
    let info = crate::ipc::InstanceInfo {
        wayland_display: crate::ipc::normalise_display(&display),
        socket: paths.socket.clone(),
        pid: std::process::id(),
        started: Utc::now().timestamp(),
    };
    paths.write_info(&info).map_err(|source| Error::Instance {
        path: paths.info.clone(),
        source,
    })?;

    log_info(&format!(
        "recording to {} (closed {dangling} dangling interval(s))",
        database.display()
    ));

    let mut daemon = Daemon::new(store, server, config, config_path.to_owned());
    daemon.start_compositor();
    if config.idle_timeout.is_zero() {
        log_info("idle detection is off (idle.timeout is 0s)");
    } else {
        daemon.start_idle(config.idle_timeout);
    }

    daemon.main_loop();
    Ok(())
}

/// Start logging.
///
/// Colours are enabled only when stderr is a terminal. Running under systemd
/// sends this to the journal, where escape codes are noise rather than help, and
/// `NO_COLOR` is honoured for people who do not want them anywhere.
pub fn init_logging() {
    use std::io::IsTerminal as _;

    let colours = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(colours)
        // The binary's own name is the only target worth printing, and it would
        // appear on every line.
        .with_target(false)
        // Local time, not the subscriber's default UTC: this is a desktop tool and
        // the reader is in the same timezone as the machine.
        .with_timer(LocalTime)
        .with_writer(std::io::stderr)
        .init();
}

/// Timestamps as `10:42:07.123`, in local time.
struct LocalTime;

impl tracing_subscriber::fmt::time::FormatTime for LocalTime {
    fn format_time(
        &self,
        writer: &mut tracing_subscriber::fmt::format::Writer<'_>,
    ) -> std::fmt::Result {
        write!(writer, "{}", chrono::Local::now().format("%H:%M:%S%.3f"))
    }
}

/// How to describe the daemon that already owns the session.
fn describe_holder(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("a daemon is already running for this session (pid {pid})"),
        None => String::from("a daemon is already running for this session"),
    }
}

/// Something that happened, worth a line in the journal.
fn log_info(message: &str) {
    tracing::info!("{message}");
}

/// Something that went wrong but did not stop the daemon.
fn log_warn(message: &str) {
    tracing::warn!("{message}");
}

/// Something that lost data.
fn log_error(message: &str) {
    tracing::error!("{message}");
}

/// The daemon's whole state.
struct Daemon {
    store: Store,
    server: Server,
    tracker: Tracker,
    config: Resolved,
    config_path: PathBuf,
    /// Events from the niri reader thread.
    events: Option<Receiver<Watch>>,
    /// The niri reader thread's handle, kept so a shutdown can join it.
    reader: Option<std::thread::JoinHandle<()>>,
    /// Idle detection, once started.
    idle: Option<Idle>,
    /// Whether the daemon should stop.
    stopping: bool,
}

impl Daemon {
    fn new(store: Store, server: Server, config: &Resolved, config_path: PathBuf) -> Self {
        Self {
            store,
            server,
            tracker: Tracker::new(config.idle_timeout, config.terminals.clone()),
            config: config.clone(),
            config_path,
            events: None,
            reader: None,
            idle: None,
            stopping: false,
        }
    }

    /// Start reading niri's event stream on its own thread.
    ///
    /// The stream is a blocking socket, so it cannot share the daemon's loop
    /// directly. A thread whose only job is to read and forward keeps that
    /// blocking contained, and a reconnect is then just that thread starting over.
    fn start_compositor(&mut self) {
        let (sender, receiver) = channel();
        let handle = std::thread::spawn(move || {
            let mut windows: Vec<niri::Window> = Vec::new();
            let mut backoff = RECONNECT_BASE;
            loop {
                // The closure cannot break the stream, so a send failure is
                // remembered and acted on once the stream ends: at that point the
                // daemon is gone and reconnecting would be pointless.
                let mut receiver_gone = false;
                let result = niri::run_event_stream(
                    |event| {
                        if let Some(watch) = niri::interpret(&event, &mut windows)
                            && sender.send(watch).is_err()
                        {
                            receiver_gone = true;
                        }
                    },
                    // A connection that succeeded resets the backoff. Without
                    // this, a stream that ran happily for hours would wait the
                    // full 30 s after a single drop, because the delay only ever
                    // grows.
                    || backoff = RECONNECT_BASE,
                );
                if receiver_gone {
                    return;
                }
                if let Err(err) = result {
                    log_warn(&format!(
                        "compositor event stream ended: {err}; reconnecting"
                    ));
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        });

        self.events = Some(receiver);
        self.reader = Some(handle);

        // The event stream starts with a full snapshot, so no separate query is
        // needed to learn the current window.
    }

    /// Start idle detection, or say why it could not be started.
    fn start_idle(&mut self, timeout: Duration) {
        match Idle::start(timeout) {
            Ok(idle) => {
                log_info(&format!(
                    "idle detection on, timeout {}",
                    crate::config::format_duration(timeout)
                ));
                self.idle = Some(idle);
            }
            Err(err) => {
                // Not fatal: recording continues, it just cannot tell a watched
                // video from an empty room. The tracker is told the same thing, so
                // it stops believing the user is away — nothing is left to report
                // that they came back.
                log_warn(&format!("idle detection unavailable: {err}"));
                self.tracker.forget_idle();
            }
        }
    }

    /// The main loop.
    fn main_loop(&mut self) {
        let clock = SystemClock;
        let mut next_tick = std::time::Instant::now() + crate::tracker::TICK;

        while !self.stopping {
            // Deliver any compositor events that have arrived. The timeout is
            // short because the checkpoints are cheap and a client waiting for
            // `status` should not be made to wait a second.
            self.drain_events();

            // Act on idle transitions.
            self.poll_idle();

            // Answer clients.
            self.serve_clients();

            // Checkpoint on the timer.
            let now = std::time::Instant::now();
            if now >= next_tick {
                self.decision(|tracker| tracker.on_tick(&clock));
                // The next deadline is counted from now, not from the one that was
                // missed. A suspend leaves the deadline hours in the past, and
                // adding would fire hundreds of catch-up ticks in a row — each one
                // a database write — for time the tracker has already decided not
                // to count.
                next_tick = now + crate::tracker::TICK;
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        // Close whatever was open so the last interval is not left dangling.
        self.decision(|tracker| tracker.pause(&clock));
        self.cleanup();
    }

    /// Read every compositor event waiting, without blocking.
    fn drain_events(&mut self) {
        // Events are collected before any of them is handled: the receiver is
        // borrowed from `self`, and handling an event needs `self` mutably.
        let mut pending = Vec::new();
        let Some(receiver) = &self.events else {
            return;
        };
        while let Ok(watch) = receiver.try_recv() {
            pending.push(watch);
        }

        let clock = SystemClock;
        for watch in pending {
            match watch {
                Watch::Focus(focus) => {
                    self.decision(|tracker| tracker.on_focus(focus, &clock));
                }
                Watch::Windows => {
                    // The window list changed; the focused window may have moved
                    // even if the focus event was not sent, so re-derive it.
                    self.refresh_focus();
                }
                Watch::Opened(focus) => {
                    // A new window: work out what it is running now, while the
                    // process it started is the newest one under its parent. Doing
                    // it here rather than on first focus is what makes several
                    // windows of one process distinguishable at all.
                    self.tracker.prefetch(&focus);
                    self.refresh_focus();
                }
                Watch::Closed(id) => {
                    self.tracker.forget_window(id);
                    self.refresh_focus();
                }
            }
        }
    }

    /// Ask niri which window is focused, and hand it to the tracker.
    ///
    /// Used after a window list change, when the focus may have moved without a
    /// focus event (a window closed and another took its place).
    fn refresh_focus(&mut self) {
        let clock = SystemClock;
        match niri::query_focused() {
            // Handed over unconditionally: comparing here would have to resolve a
            // terminal to its foreground process the same way the tracker does, and
            // getting that subtly different is how intervals get sliced. The tracker
            // ignores a focus it is already timing.
            Ok(focus) => self.decision(|tracker| tracker.on_focus(focus, &clock)),
            Err(err) => log_warn(&format!("cannot query the focused window: {err}")),
        }
    }

    /// Act on idle transitions since the last call.
    fn poll_idle(&mut self) {
        let Some(idle) = &mut self.idle else {
            return;
        };
        let events = match idle.poll() {
            Ok(events) => events,
            Err(err) => {
                log_warn(&format!("idle detection stopped: {err}"));
                self.idle = None;
                return;
            }
        };
        let clock = SystemClock;

        // Settle the inhibitor before acting on idleness: whether an idle stretch
        // counts depends on it, and the derived answer needs both notifications to
        // have been dispatched.
        let inhibited = idle.state().inhibited();
        if inhibited != self.tracker.inhibited() {
            log_info(&format!("idle: inhibitor {inhibited}"));
            self.decision(|tracker| tracker.on_inhibitor(inhibited, &clock));
        }

        for event in events {
            match event {
                IdleEvent::InputIdle => {
                    // The event arrives one timeout after input stopped, so the
                    // instant to cut at is one timeout back.
                    let idle_for =
                        chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default();
                    log_info(&format!(
                        "idle: input stopped {} ago",
                        crate::config::format_duration(self.config.idle_timeout)
                    ));
                    self.decision(|tracker| tracker.on_input_idle(Utc::now() - idle_for, &clock));
                }
                IdleEvent::InputActive => {
                    log_info("idle: input resumed");
                    self.decision(|tracker| tracker.on_input_active(&clock));
                }
                // Not logged. The inhibitor-aware notification also fires whenever
                // input stops or resumes, so reporting the event itself would print
                // a line that means nothing on every transition. The derived state
                // above is logged instead, and only when it actually changes.
                IdleEvent::InhibitorChanged => {}
            }
        }
    }

    /// Ask the tracker what to do, then do it.
    ///
    /// The two steps are separate methods because the tracker and the store are
    /// both borrowed mutably from `self`; computing the whole decision first keeps
    /// those borrows from overlapping.
    fn decision(&mut self, decide: impl FnOnce(&mut Tracker) -> Vec<Effect>) {
        let effects = decide(&mut self.tracker);
        self.apply(effects);
    }

    /// Answer every client waiting on the control socket.
    ///
    /// Acceptance is finished before answering starts, so the listener's borrow
    /// has ended by the time the state machine and the database are touched.
    fn serve_clients(&mut self) {
        loop {
            let accepted = match self.server.accept() {
                Ok(accepted) => accepted,
                Err(err) => {
                    log_warn(&format!("control socket error: {err}"));
                    return;
                }
            };
            let Some(mut incoming) = accepted else {
                return;
            };
            let stream = &mut incoming.stream;

            // A line that was not a request still gets an answer: the client is
            // waiting on this connection either way, and telling it what was wrong
            // is more use than dropping the socket.
            match incoming.request {
                Ok(request) => {
                    let response = self.handle(&request);
                    if let Err(err) = crate::ipc::server::reply(stream, &response) {
                        log_warn(&format!("cannot reply to a client: {err}"));
                    }
                }
                Err(message) => {
                    if let Err(err) = crate::ipc::server::reply_parse_failure(stream, message) {
                        log_warn(&format!("cannot reply to a client: {err}"));
                    }
                }
            }
        }
    }

    /// Apply the changes a tracker decision asks for.
    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Open(live) => match self.store.begin(&live) {
                    Ok(id) => self.tracker.adopt(id),
                    Err(err) => {
                        log_error(&format!("cannot record an interval: {err}"));
                        // The row does not exist, so nothing can extend or close it.
                        // Dropping the tracker's copy means the next focus event
                        // starts again instead of addressing an id that is not there.
                        self.tracker.abandon_open();
                    }
                },
                Effect::Extend { id, end } => {
                    if let Err(err) = self.store.extend(id, end) {
                        log_error(&format!("cannot extend an interval: {err}"));
                    }
                }
                Effect::Close { id, end } => {
                    if let Err(err) = self.store.close(id, end) {
                        log_error(&format!("cannot close an interval: {err}"));
                    }
                }
                Effect::MarkPassive { id } => {
                    if let Err(err) = self.store.mark_passive(id) {
                        log_error(&format!("cannot mark an interval passive: {err}"));
                    }
                }
            }
        }
    }

    /// Stop the clock and say goodbye.
    fn cleanup(&mut self) {
        log_info("stopping");
        // The reader thread is parked inside a blocking read; the process exiting
        // is what ends it, so it is deliberately not joined.
        self.reader = None;
    }

    /// Re-read the configuration.
    fn reload(&mut self) -> Result<(), String> {
        match crate::config::load_from(&self.config_path) {
            Ok(resolved) => {
                // Finish the open row under the settings it was recorded with, then
                // adopt the new ones. The tracker is reconfigured rather than
                // rebuilt so that paused and idle survive: a fresh machine would
                // believe recording is running and the user is present, and would
                // start counting both.
                let clock = SystemClock;
                self.decision(|tracker| tracker.close_open(&clock));

                self.config = resolved.clone();
                self.tracker
                    .reconfigure(resolved.idle_timeout, resolved.terminals.clone());
                // Re-opens an interval unless paused, and stores the focus without
                // one while idle, which is exactly the state to resume from.
                self.refresh_focus();
                // Restart idle detection so a changed timeout takes effect.
                self.idle = None;
                if !resolved.idle_timeout.is_zero() {
                    self.start_idle(resolved.idle_timeout);
                }
                log_info("configuration reloaded");
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// The daemon's own status.
    fn status(&self) -> Status {
        // A failed count reads as "the database is empty", which is the opposite of
        // what it means. The number stays zero — the wire format has no room for an
        // error — but the journal records that it is not a real zero.
        let intervals = match self.store.count() {
            Ok(count) => count,
            Err(err) => {
                log_warn(&format!("cannot count intervals: {err}"));
                0
            }
        };

        Status {
            paused: self.tracker.paused(),
            live: self.tracker.live().map(|live| {
                let mut status = LiveStatus::from(live);
                status.seconds = live.elapsed(Utc::now()).num_seconds();
                status
            }),
            pid: std::process::id(),
            intervals,
        }
    }

    /// Build a report or details answer.
    fn query(&self, args: &QueryArgs, details: bool) -> Result<Report, WireError> {
        let sessions = self
            .store
            .range(args.from, args.to)
            .map_err(|err| WireError {
                kind: ErrorKind::Storage,
                message: err.to_string(),
            })?;

        // `details` is a report grouped by title for one app; the query already
        // carries the app filter, so the only difference is the grouping.
        let mut query = args.clone();
        if details {
            query.by = GroupKey::Title;
        }

        Ok(report::build(
            &sessions,
            self.tracker.live(),
            &query,
            &self.config,
        ))
    }
}

impl Daemon {
    fn handle(&mut self, request: &Request) -> Response {
        let id = request.id;
        let ok = |result: Payload| Response::Ok { id, result };
        let failed = |kind: ErrorKind, message: String| Response::Failed {
            id,
            error: WireError { kind, message },
        };

        match request.verb {
            Verb::Status => ok(Payload::Status(self.status())),
            Verb::Pause => {
                let clock = SystemClock;
                self.decision(|tracker| tracker.pause(&clock));
                ok(Payload::Empty(Empty))
            }
            Verb::Resume => {
                self.decision(|tracker| tracker.resume());
                // Nothing is focused as far as the tracker knows until niri says
                // so, and asking is the only way to start counting again promptly.
                self.refresh_focus();
                ok(Payload::Empty(Empty))
            }
            Verb::Reload => match self.reload() {
                Ok(()) => ok(Payload::Empty(Empty)),
                Err(message) => failed(ErrorKind::BadRequest, message),
            },
            Verb::Quit => {
                self.stopping = true;
                ok(Payload::Empty(Empty))
            }
            Verb::Report | Verb::Details => {
                let Some(crate::ipc::Args::Query(args)) = &request.args else {
                    return failed(ErrorKind::BadRequest, String::from("report needs a query"));
                };
                match self.query(args, request.verb == Verb::Details) {
                    Ok(report) => ok(Payload::Report(report)),
                    Err(error) => failed(error.kind, error.message),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ipc::Args;
    use chrono::NaiveDate;

    fn config() -> Resolved {
        let preset: Config = toml::from_str(crate::config::PRESET).expect("preset parses");
        preset.resolve().expect("preset resolves")
    }

    fn daemon() -> Daemon {
        // Tests run in parallel inside one process, so the socket path has to be
        // unique per test; a shared name would make them race for the bind.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let store = Store::open_in_memory().expect("in-memory store");
        let dir = std::env::temp_dir().join(format!("nt-daemon-{}-{unique}", std::process::id()));
        let server = Server::bind(&dir.join("test.sock")).expect("binds");
        Daemon::new(store, server, &config(), dir.join("config.toml"))
    }

    fn request(verb: Verb, args: Option<Args>) -> Request {
        Request { id: 1, verb, args }
    }

    #[test]
    fn status_reports_an_empty_daemon() {
        let mut daemon = daemon();
        let response = daemon.handle(&request(Verb::Status, None));
        match response {
            Response::Ok {
                result: Payload::Status(status),
                ..
            } => {
                assert!(!status.paused);
                assert!(status.live.is_none());
            }
            other => panic!("expected a status, got {other:?}"),
        }
    }

    #[test]
    fn pause_makes_status_say_so() {
        let mut daemon = daemon();
        daemon.handle(&request(Verb::Pause, None));
        match daemon.handle(&request(Verb::Status, None)) {
            Response::Ok {
                result: Payload::Status(status),
                ..
            } => assert!(status.paused),
            other => panic!("expected a status, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_report_has_no_rows() {
        let mut daemon = daemon();
        let args = Args::Query(QueryArgs {
            from: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            by: GroupKey::App,
            app: None,
            title: None,
            exclude_passive: false,
            limit: None,
        });
        match daemon.handle(&request(Verb::Report, Some(args))) {
            Response::Ok {
                result: Payload::Report(report),
                ..
            } => {
                assert!(report.rows.is_empty());
                assert_eq!(report.total_seconds, 0);
            }
            other => panic!("expected a report, got {other:?}"),
        }
    }

    #[test]
    fn a_report_without_a_query_is_refused() {
        let mut daemon = daemon();
        match daemon.handle(&request(Verb::Report, None)) {
            Response::Failed { error, .. } => assert_eq!(error.kind, ErrorKind::BadRequest),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn quit_asks_the_loop_to_stop() {
        let mut daemon = daemon();
        daemon.handle(&request(Verb::Quit, None));
        assert!(daemon.stopping);
    }
}
