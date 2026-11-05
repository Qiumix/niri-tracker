//! The daemon end of the control socket.
//!
//! One connection carries exactly one request, answered by one line. The
//! daemon's own loop decides when to poll: [`serve_pending`] accepts whatever is
//! waiting and returns, so it can be called from the same loop that dispatches
//! compositor events.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};

use super::protocol::{Request, Response};

/// Upper bound on one request line. Generous for the protocol, small enough that
/// a hostile client cannot make the daemon allocate without limit.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// What the daemon does with a request.
///
/// Implemented by the daemon, which owns the state machine.
pub trait Handler {
	/// Execute one request.
	fn handle(&mut self, request: &Request) -> Response;
}

/// A bound socket awaiting connections.
pub struct Server {
	listener: UnixListener,
}

impl Server {
	/// Bind the socket at `path`, replacing a stale one.
	///
	/// The socket is chmod `0600`: same-user processes are the intended clients,
	/// but nobody else should reach the daemon.
	pub fn bind(path: &std::path::Path) -> std::io::Result<Self> {
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		// A previous instance may have died without cleaning up. Its socket
		// cannot be reused, so remove it first; the caller has already
		// established that no live instance owns this path.
		let _ = std::fs::remove_file(path);

		let listener = UnixListener::bind(path)?;
		{
			use std::os::unix::fs::PermissionsExt as _;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		listener.set_nonblocking(true)?;
		Ok(Self { listener })
	}

	/// Accept and answer every connection that is currently waiting.
	///
	/// Returns how many were answered, so the caller can tell whether there was
	/// anything to do.
	pub fn serve_pending(&self, handler: &mut dyn Handler) -> std::io::Result<usize> {
		let mut served = 0;
		loop {
			match self.listener.accept() {
				Ok((stream, _address)) => {
					// One bad client must not take the daemon down, so a failure
					// to read or write is swallowed and the next client served.
					let _ = answer(stream, handler);
					served += 1;
				}
				Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
				Err(err) => return Err(err),
			}
		}
		Ok(served)
	}
}

/// Read one request from `stream` and write one response back.
fn answer(stream: UnixStream, handler: &mut dyn Handler) -> std::io::Result<()> {
	stream.set_nonblocking(false)?;
	let reader = BufReader::new(stream.try_clone()?);

	let mut line = String::new();
	// A limit is enforced by reading at most this many bytes, rather than by
	// trusting the client to send a short line.
	let mut limited = reader.take(MAX_REQUEST_BYTES);
	limited.read_line(&mut line)?;

	let response = match serde_json::from_str::<Request>(&line) {
		Ok(request) => handler.handle(&request),
		Err(err) => Response::Failed {
			// Without a parseable request there is no id to echo. Zero is never
			// assigned by a client, so it reads as "not yours".
			id: 0,
			error: super::protocol::WireError {
				kind: super::protocol::ErrorKind::BadRequest,
				message: err.to_string(),
			},
		},
	};

	let mut writer = stream;
	writer.write_all(super::protocol::encode_line(&response)?.as_bytes())?;
	writer.flush()?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ipc::protocol::{ErrorKind, Payload, Verb};

	struct Echo;

	impl Handler for Echo {
		fn handle(&mut self, request: &Request) -> Response {
			if request.verb == Verb::Quit {
				return Response::Failed {
					id: request.id,
					error: super::super::protocol::WireError {
						kind: ErrorKind::Unavailable,
						message: String::from("nope"),
					},
				};
			}
			Response::Ok {
				id: request.id,
				result: Payload::Empty(super::super::protocol::Empty),
			}
		}
	}

	fn round_trip(path: &std::path::Path, line: &str) -> String {
		let server = Server::bind(path).expect("binds");
		let mut client = UnixStream::connect(path).expect("connects");
		client.write_all(line.as_bytes()).expect("writes");
		client
			.shutdown(std::net::Shutdown::Write)
			.expect("shuts down");

		let mut handler = Echo;
		server.serve_pending(&mut handler).expect("serves");

		let mut reply = String::new();
		BufReader::new(client).read_line(&mut reply).expect("reads");
		reply
	}

	#[test]
	fn a_request_is_answered_on_one_line() {
		let dir = std::env::temp_dir().join(format!("nt-ipc-{}", std::process::id()));
		let path = dir.join("1.sock");

		let reply = round_trip(&path, "{\"id\":5,\"verb\":\"status\"}\n");
		assert_eq!(reply.matches('\n').count(), 1);
		let parsed: Response = serde_json::from_str(&reply).expect("decodes");
		assert_eq!(parsed.id(), 5);

		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn nonsense_gets_a_bad_request_reply() {
		let dir = std::env::temp_dir().join(format!("nt-ipc-bad-{}", std::process::id()));
		let path = dir.join("1.sock");

		let reply = round_trip(&path, "not json\n");
		let parsed: Response = serde_json::from_str(&reply).expect("decodes");
		match parsed {
			Response::Failed { error, .. } => assert_eq!(error.kind, ErrorKind::BadRequest),
			Response::Ok { .. } => panic!("expected a failure"),
		}

		let _ = std::fs::remove_dir_all(&dir);
	}
}
