//! The client end of the control socket.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;

use super::protocol::{Args, Request, Response, Verb};

/// Why a request could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// The daemon is not running, or its socket is unreachable.
	#[error("no running niri-tracker daemon: {0}")]
	Unreachable(#[from] std::io::Error),

	/// The daemon answered with something that is not a valid response.
	#[error("the daemon sent an unreadable reply: {0}")]
	Malformed(#[from] serde_json::Error),

	/// The daemon refused the request.
	#[error("{message}")]
	Refused {
		/// A short machine-stable tag.
		kind: super::protocol::ErrorKind,
		/// A sentence for a human.
		message: String,
	},
}

/// An open connection to a daemon.
pub struct Client {
	socket: std::path::PathBuf,
}

impl Client {
	/// Connect to the daemon at `socket`.
	pub fn connect(socket: &Path) -> Result<Self, Error> {
		// Connecting immediately proves reachability, so callers learn about a
		// dead daemon before they build a request around the result.
		std::os::unix::net::UnixStream::connect(socket)?;
		Ok(Self {
			socket: socket.to_owned(),
		})
	}

	/// Send one request and read its answer.
	pub fn call(&self, request: &Request) -> Result<Response, Error> {
		let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
		stream.write_all(super::protocol::encode_line(request)?.as_bytes())?;
		stream.shutdown(std::net::Shutdown::Write)?;

		let mut reply = String::new();
		BufReader::new(stream).read_line(&mut reply)?;

		let response: Response = serde_json::from_str(&reply)?;
		match response {
			Response::Failed { error, .. } => Err(Error::Refused {
				kind: error.kind,
				message: error.message,
			}),
			Response::Ok { .. } => Ok(response),
		}
	}

	/// Send a request that takes no arguments.
	pub fn simple(&self, verb: Verb) -> Result<Response, Error> {
		self.call(&Request {
			id: 1,
			verb,
			args: None,
		})
	}

	/// Send a request with a query payload.
	pub fn query(&self, verb: Verb, args: Args) -> Result<Response, Error> {
		self.call(&Request {
			id: 1,
			verb,
			args: Some(args),
		})
	}
}
