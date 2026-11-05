//! The local control protocol.
//!
//! Three pieces, deliberately independent:
//!
//! - [`protocol`] is the wire format: NDJSON requests and responses.
//! - [`discovery`] finds the socket belonging to a given Wayland session.
//! - [`server`] and [`client`] are the two ends of a connection.
//!
//! Everything here is synchronous. The daemon's main loop polls the compositor;
//! adding an async runtime to move a few lines of JSON per minute would cost more
//! than it saves.

pub mod client;
pub mod discovery;
pub mod protocol;
pub mod server;

pub use client::Client;
pub use discovery::{Discovered, InstanceInfo, InstancePaths, discover, normalise_display};
pub use protocol::{
	Args, Empty, ErrorKind, GroupKey, LiveStatus, Payload, QueryArgs, Report, Request, Response,
	Row, Status, Verb, WireError, encode_line,
};
pub use server::Server;
