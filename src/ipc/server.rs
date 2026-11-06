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
use std::time::Duration;

use super::protocol::{Request, Response};

/// Upper bound on one request line. Generous for the protocol, small enough that
/// a hostile client cannot make the daemon allocate without limit.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// How long a client may take to send its one line.
///
/// The request is read on the same thread that records intervals, so a local
/// client that connects and then says nothing would otherwise stall the daemon
/// for as long as it cared to hold the socket open. Nothing legitimate takes
/// anywhere near this long to write a single line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Returns what was read together with the stream it arrived on, so the caller
    /// can answer after it has finished borrowing everything else. `None` means
    /// nobody is waiting.
    ///
    /// A line that is not a request comes back as an error rather than as no
    /// connection at all: the client is still there, waiting for an answer, and
    /// deserves to be told what was wrong with it. Only a connection that sends
    /// nothing is dropped.
    pub fn accept(&self) -> std::io::Result<Option<Incoming>> {
        match self.listener.accept() {
            Ok((stream, _address)) => match read_request(&stream) {
                Some(request) => Ok(Some(Incoming { request, stream })),
                None => Ok(None),
            },
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// One connection, ready to be answered.
pub struct Incoming {
    /// The request, or why the line that arrived was not one.
    pub request: Result<Request, String>,
    /// The connection the answer goes back on.
    pub stream: UnixStream,
}

/// Write a response to `stream` and close it.
pub fn reply(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    stream.write_all(super::protocol::encode_line(response)?.as_bytes())?;
    stream.flush()
}

/// Read one request from a fresh connection.
///
/// `None` means nothing was sent, which is not worth answering. `Some(Err(..))`
/// means a line arrived but was not a request: the client is waiting and gets an
/// explanation.
fn read_request(stream: &UnixStream) -> Option<Result<Request, String>> {
    read_request_within(stream, REQUEST_TIMEOUT)
}

/// The same, with the timeout given, so tests need not wait five seconds.
fn read_request_within(stream: &UnixStream, timeout: Duration) -> Option<Result<Request, String>> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    let reader = BufReader::new(stream.try_clone().ok()?);

    let mut line = String::new();
    // A limit is enforced by reading at most this many bytes, rather than by
    // trusting the client to send a short line.
    let mut limited = reader.take(MAX_REQUEST_BYTES);
    limited.read_line(&mut line).ok()?;

    if line.trim().is_empty() {
        return None;
    }

    Some(serde_json::from_str::<Request>(&line).map_err(|err| err.to_string()))
}

/// Answer a line that could not be parsed.
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

        let mut incoming = server.accept().expect("accepts").expect("has a client");
        let request = incoming.request.expect("a well-formed request");
        let response = Response::Ok {
            id: request.id,
            result: Payload::Empty(Empty),
        };
        reply(&mut incoming.stream, &response).expect("replies");

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
    fn a_client_that_never_speaks_does_not_block_the_daemon() {
        // The request is read on the thread that records intervals, so a client
        // that connects and says nothing must not be able to hold it.
        let (ours, _silent) = UnixStream::pair().expect("socket pair");
        let started = std::time::Instant::now();

        assert!(
            read_request_within(&ours, Duration::from_millis(50)).is_none(),
            "a silent client yields no request"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "reading must give up after the timeout, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nonsense_is_answered_rather_than_dropped() {
        let dir = temp_dir("bad");
        let path = dir.join("1.sock");
        let server = Server::bind(&path).expect("binds");

        let mut client = UnixStream::connect(&path).expect("connects");
        client.write_all(b"not json\n").expect("writes");
        client
            .shutdown(std::net::Shutdown::Write)
            .expect("shuts down");

        // The line arrives but is not a request: the client is still waiting, so it
        // has to be answered rather than left with a closed socket and no idea why.
        let mut incoming = server.accept().expect("accepts").expect("has a client");
        let message = incoming.request.expect_err("not a request");
        reply_parse_failure(&mut incoming.stream, message).expect("replies");

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
