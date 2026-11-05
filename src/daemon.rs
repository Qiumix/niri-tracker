//! The daemon: one loop that watches the compositor, records intervals, and
//! answers clients.
//!
//! Everything runs on one thread. The event sources are all file descriptors
//! (niri's socket, the Wayland connection, the IPC listener) plus a one-minute
//! timer, so a single `poll` over them is enough; spreading the work over threads
//! would add synchronisation to a program that handles a few events a minute.
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

use crate::compositor::idle::{Idle, IdleEvent, IdleState};
use crate::compositor::niri::{self, Watch};
use crate::config::Resolved;
use crate::db::{Live, Store};
use crate::ipc::{
	Empty, ErrorKind, GroupKey, LiveStatus, Payload, QueryArgs, Report, Request, Response, Server,
	Status, Verb, WireError,
};
use crate::report;
use crate::tracker::{Effect, SystemClock, Tracker};

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
	#[error("a daemon is already running for this session (pid {pid})")]
	AlreadyRunning {
		/// The pid of the instance that owns the session.
		pid: u32,
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

	/// The compositor connection failed in a way that is not recoverable.
	#[error("compositor connection failed: {0}")]
	Compositor(String),
}

/// Run the daemon until asked to stop.
pub fn run(config: &Resolved, config_path: &Path) -> Result<(), Error> {
	let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| String::from("wayland-0"));
	let runtime_dir = crate::config::runtime_dir();

	// Refuse to be the second daemon for one session: two of them would record
	// every second twice, and nothing about the resulting report would look wrong.
	for found in crate::ipc::discover(&runtime_dir, &display) {
		if found.alive {
			return Err(Error::AlreadyRunning {
				pid: found.info.pid,
			});
		}
		// A leftover from a daemon that died; safe to remove now that it is known
		// not to be listening.
		crate::ipc::InstancePaths::for_pid(&runtime_dir, found.info.pid).cleanup();
	}

	let store = Store::open(&crate::config::database_file())?;
	// Rows left open belong to a process that is gone. Their end is the last
	// checkpoint, so finishing them is all that is needed.
	let dangling = store.close_dangling()?;

	let paths = crate::ipc::InstancePaths::for_pid(&runtime_dir, std::process::id());
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

	// The instance files must go away however the daemon ends, including a panic.
	struct Cleanup(PathBuf);
	impl Drop for Cleanup {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}
	let _cleanup = Cleanup(runtime_dir.clone());

	log_info(&format!(
		"recording to {} (closed {dangling} dangling interval(s))",
		crate::config::database_file().display()
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

/// Print a line for the journal.
fn log_info(message: &str) {
	eprintln!("niri-tracker: {message}");
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
	/// The last idle state acted on, so only changes cause work.
	last_idle: IdleState,
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
			last_idle: IdleState::default(),
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
			let mut backoff = Duration::from_secs(1);
			loop {
				// The closure cannot break the stream, so a send failure is
				// remembered and acted on once the stream ends: at that point the
				// daemon is gone and reconnecting would be pointless.
				let mut receiver_gone = false;
				let result = niri::run_event_stream(|event| {
					if let Some(watch) = niri::interpret(&event, &mut windows)
						&& sender.send(watch).is_err()
					{
						receiver_gone = true;
					}
				});
				if receiver_gone {
					return;
				}
				if let Err(err) = result {
					log_info(&format!(
						"compositor event stream ended: {err}; reconnecting"
					));
				}
				std::thread::sleep(backoff);
				backoff = (backoff * 2).min(Duration::from_secs(30));
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
				// video from an empty room.
				log_info(&format!("idle detection unavailable: {err}"));
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
			if std::time::Instant::now() >= next_tick {
				self.decision(|tracker| tracker.on_tick(&clock));
				next_tick += crate::tracker::TICK;
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
			Ok(focus) => {
				if self
					.tracker
					.live()
					.map(|live| (live.app_id.clone(), live.title.clone()))
					!= focus
						.as_ref()
						.map(|focus| (focus.app_id.clone(), focus.title.clone()))
				{
					self.decision(|tracker| tracker.on_focus(focus, &clock));
				}
			}
			Err(err) => log_info(&format!("cannot query the focused window: {err}")),
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
				log_info(&format!("idle detection stopped: {err}"));
				self.idle = None;
				return;
			}
		};
		let state = idle.state().clone();
		let clock = SystemClock;

		for event in events {
			match event {
				IdleEvent::InputIdle => {
					// The event arrives one timeout after input stopped, so the
					// instant to cut at is one timeout back.
					let idled_at = Utc::now()
						- chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default();
					self.decision(|tracker| tracker.on_input_idle(idled_at, &clock));
				}
				IdleEvent::InputActive => {
					self.decision(|tracker| tracker.on_input_active(&clock));
				}
				IdleEvent::Inhibited | IdleEvent::Uninhibited => {
					let inhibited = state.inhibited();
					self.decision(|tracker| tracker.on_inhibitor(inhibited, &clock));
				}
			}
		}
		self.last_idle = state;
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
					log_info(&format!("control socket error: {err}"));
					return;
				}
			};
			let Some((request, mut stream)) = accepted else {
				return;
			};

			let response = self.answer(&request);
			if let Err(err) = crate::ipc::server::reply(&mut stream, &response) {
				log_info(&format!("cannot reply to a client: {err}"));
			}
		}
	}

	/// Decide what a request means.
	fn answer(&mut self, request: &Request) -> Response {
		self.handle(request)
	}

	/// Apply the changes a tracker decision asks for.
	fn apply(&mut self, effects: Vec<Effect>) {
		for effect in effects {
			match effect {
				Effect::Open(live) => match self.store.begin(&live) {
					Ok(id) => self.tracker.adopt(id),
					Err(err) => log_info(&format!("cannot record an interval: {err}")),
				},
				Effect::Extend { id, end } => {
					if let Err(err) = self.store.extend(id, end) {
						log_info(&format!("cannot extend an interval: {err}"));
					}
				}
				Effect::Close { id, end } => {
					if let Err(err) = self.store.close(id, end) {
						log_info(&format!("cannot close an interval: {err}"));
					}
				}
				Effect::Trim { id, start } => {
					if let Err(err) = self.store.set_start(id, start) {
						log_info(&format!("cannot trim an interval: {err}"));
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
				self.config = resolved.clone();
				self.tracker = Tracker::new(resolved.idle_timeout, resolved.terminals.clone());
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
		Status {
			paused: self.tracker.paused(),
			live: self.tracker.live().map(|live| {
				let mut status = LiveStatus::from(live);
				status.seconds = live.elapsed(Utc::now()).num_seconds();
				status
			}),
			pid: std::process::id(),
			intervals: self.store.count().unwrap_or(0),
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

/// A stretch being recorded, for callers that need one without a store.
#[must_use]
pub fn empty_live() -> Live {
	Live {
		app_id: String::new(),
		title: String::new(),
		passive: false,
		start: Utc::now(),
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
