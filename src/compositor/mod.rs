//! Talking to the compositor.
//!
//! Two independent connections, for two independent jobs:
//!
//! - [`niri`] watches which window is focused, over niri's IPC socket.
//! - [`idle`] watches whether the user is still touching the keyboard and mouse,
//!   over a Wayland connection using `ext-idle-notify-v1`.
//!
//! They are separate because they have to be: niri's IPC carries no input
//! timestamps at all, so idleness can only come from the Wayland protocol.
//!
//! Idle detection is optional. A daemon that cannot start it still records
//! everything, it just cannot tell a watched video from an empty room.

pub mod idle;
pub mod niri;

pub use crate::tracker::Focus;
pub use idle::{Idle, IdleEvent, IdleState, Input};
pub use niri::{Watch, Window};
