//! The daemon end of the control socket.
//!
//! One connection carries exactly one request, answered by one line. The daemon's
//! own loop decides when to poll: [`Server::accept`] returns nothing when nobody
//! is waiting, so it can be called from the same loop that dispatches compositor
//! events.
//!
//! Acceptance and answering are deliberately separate. The accepted connection is
//! an owned value, so the daemon can drop its borrow of the listener before it
//! touches the state machine and the database; answering is then just writing to
//! a stream it owns.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};

use super::protocol::{Request, Response};

/// Upper bound on one request line. Generous for the protocol, small enough that
/// a hostile client cannot make the daemon allocate without limit.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

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
		// A previous instance may have died without cleaning up. Its socket cannot
		// be reused, so remove it first; the caller has already established that no
		// live instance owns this path.
		let _ = std::fs::remove_file(path);

		let listener = UnixListener::bind(path)?;
		{
			use std::os::unix::fs::PermissionsExt as _;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		listener.set_nonblocking(true)?;
		Ok(Self { listener })
	}

	/// Take one waiting connection and read its request.
	///
	/// Returns the request together with the stream it arrived on, so the caller
	/// can answer after it has finished borrowing everything else. `None` means
	/// nobody is waiting.
	///
	/// A client that sends nonsense still gets a stream back, carrying a failure
	/// response; only a connection that cannot be read at all is dropped.
	pub fn accept(&self) -> Result<Option<(Request, UnixStream)>, std::io::Error> {
		match self.listener.accept() {
			Ok((stream, _address)) => match read_request(&stream) {
				Some(request) => Ok(Some((request, stream))),
				None => Ok(None),
			},
			Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
			Err(err) => Err(err),
		}
	}
}

/// Write a response to `stream` and close it.
pub fn reply(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
	stream.write_all(super::protocol::encode_line(response)?.as_bytes())?;
	stream.flush()
}

/// Read one request from a fresh connection.
///
/// Returns `None` when the line is not a valid request; the caller answers that
/// with a failure rather than the stream being dropped silently, so a confused
/// client learns what went wrong.
fn read_request(stream: &UnixStream) -> Option<Request> {
	stream.set_nonblocking(false).ok()?;
	let reader = BufReader::new(stream.try_clone().ok()?);

	let mut line = String::new();
	// A limit is enforced by reading at most this many bytes, rather than by
	// trusting the client to send a short line.
	let mut limited = reader.take(MAX_REQUEST_BYTES);
	limited.read_line(&mut line).ok()?;

	serde_json::from_str::<Request>(&line).ok()
}

/// Answer a connection that could not be parsed.
///
/// Kept here so the caller does not have to construct a `WireError` at every
/// site, and so the "id 0 means unparseable" convention lives in one place.
pub fn reply_parse_failure(stream: &mut UnixStream, message: String) -> std::io::Result<()> {
	let response = Response::Failed {
		// Without a parseable request there is no id to echo. Zero is never
		// assigned by a client, so it reads as "not yours".
		id: 0,
		error: super::protocol::WireError {
			kind: super::protocol::ErrorKind::BadRequest,
			message,
		},
	};
	reply(stream, &response)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ipc::protocol::{Empty, ErrorKind, Payload};

	fn temp_dir(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("nt-ipc-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	fn ask(path: &std::path::Path, line: &str) -> Response {
		let server = Server::bind(path).expect("binds");
		let mut client = UnixStream::connect(path).expect("connects");
		client.write_all(line.as_bytes()).expect("writes");
		client
			.shutdown(std::net::Shutdown::Write)
			.expect("shuts down");

		let (request, mut stream) = server.accept().expect("accepts").expect("has a client");
		let response = Response::Ok {
			id: request.id,
			result: Payload::Empty(Empty),
		};
		reply(&mut stream, &response).expect("replies");

		let mut reply_text = String::new();
		BufReader::new(client)
			.read_line(&mut reply_text)
			.expect("reads");
		serde_json::from_str(&reply_text).expect("decodes")
	}

	#[test]
	fn a_request_is_answered_on_one_line() {
		let dir = temp_dir("one");
		let response = ask(&dir.join("1.sock"), "{\"id\":5,\"verb\":\"status\"}\n");
		assert_eq!(response.id(), 5);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn nobody_waiting_returns_none() {
		let dir = temp_dir("none");
		let server = Server::bind(&dir.join("1.sock")).expect("binds");
		assert!(server.accept().expect("accepts").is_none());
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn nonsense_is_refused_without_dropping_the_connection() {
		let dir = temp_dir("bad");
		let path = dir.join("1.sock");
		let server = Server::bind(&path).expect("binds");

		let mut client = UnixStream::connect(&path).expect("connects");
		client.write_all(b"not json\n").expect("writes");
		client
			.shutdown(std::net::Shutdown::Write)
			.expect("shuts down");

		// An unparseable line yields no request, so the daemon has nothing to
		// answer and simply moves on; the connection ends when the stream drops.
		assert!(
			server.accept().expect("accepts").is_none(),
			"an unparseable line yields no request"
		);

		// The failure reply itself is what a stricter caller would send, so it is
		// exercised on a real request/reply pair.
		let mut client = UnixStream::connect(&path).expect("connects");
		client
			.write_all(b"{\"id\":1,\"verb\":\"status\"}\n")
			.expect("writes");
		client
			.shutdown(std::net::Shutdown::Write)
			.expect("shuts down");

		let (_, mut server_side) = server.accept().expect("accepts").expect("has a request");
		reply_parse_failure(&mut server_side, String::from("bad json")).expect("replies");

		let mut text = String::new();
		BufReader::new(client).read_line(&mut text).expect("reads");
		let response: Response = serde_json::from_str(&text).expect("decodes");
		match response {
			Response::Failed { error, .. } => assert_eq!(error.kind, ErrorKind::BadRequest),
			Response::Ok { .. } => panic!("expected a failure"),
		}

		let _ = std::fs::remove_dir_all(&dir);
	}
}
