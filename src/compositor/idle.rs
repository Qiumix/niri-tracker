//! Watching for input idleness, through `ext-idle-notify-v1`.
//!
//! niri exposes this protocol (it is built on smithay's `IdleNotifierState`),
//! and it is the only supported way for a client to learn that the user has
//! stopped touching the keyboard and mouse. Polling is not an option: niri's IPC
//! carries no input timestamps, and `focus_timestamp` is a debounced
//! most-recently-used marker for alt-tab, not the last input time.
//!
//! Two notifications are created, and the difference between them is the whole
//! point:
//!
//! - `get_idle_notification` is **inhibited** by an active
//!   `zwp_idle_inhibitor_v1`. A video player holding the screen awake therefore
//!   keeps it from ever firing.
//! - `get_input_idle_notification` (protocol version 2) ignores inhibitors and
//!   fires strictly on real input going quiet.
//!
//! Comparing the two tells apart "the user left" from "the user is watching
//! something": input idle fired while the inhibited one did not means a client
//! is holding an inhibitor. That is what lets the tracker keep counting during a
//! video without also counting an unattended machine.
//!
//! The two notifications are told apart by their user data, a distinct marker
//! type each, because the protocol gives the objects no other identity: they send
//! the same two events.

use std::time::Duration;

use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    Event as NotificationEvent, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// The name a compositor advertises `ext-idle-notify-v1` under.
const NOTIFIER_INTERFACE: &str = "ext_idle_notifier_v1";

/// The name a compositor advertises seats under.
const SEAT_INTERFACE: &str = "wl_seat";

/// Version 2 added `get_input_idle_notification`, without which the inhibitor
/// comparison this module is built on is impossible.
const REQUIRED_VERSION: u32 = 2;

/// Why idle detection could not be started.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Wayland connection could not be opened.
    #[error("cannot connect to Wayland: {0}")]
    Connect(String),

    /// The compositor does not offer `ext-idle-notify-v1`, or offers it below
    /// version 2.
    #[error("the compositor does not support ext-idle-notify-v1 version {REQUIRED_VERSION}")]
    Unsupported,

    /// No seat was advertised.
    #[error("the compositor advertised no seat")]
    NoSeat,

    /// The protocol exchange failed.
    #[error("Wayland protocol error: {0}")]
    Protocol(String),
}

/// User data marking the inhibitor-aware notification.
struct InhibitedMarker;

/// User data marking the input-only notification.
struct InputMarker;

/// What the user is doing, as far as the protocols can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The user is active.
    Active,
    /// Input has been quiet for the timeout.
    Idle,
}

/// A transition worth reacting to, in the order it happened.
///
/// The inhibitor variants carry the fact directly rather than leaving the caller
/// to derive it from [`IdleState::inhibited`]: the two notifications arrive in
/// whichever order the compositor chooses, so "input has not gone quiet yet but
/// an inhibitor just appeared" is a real state that the derived answer gets
/// wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// Input went quiet after the timeout.
    InputIdle,
    /// Input came back.
    InputActive,
    /// The inhibitor-aware notification fired, meaning no inhibitor is held.
    Uninhibited,
    /// The inhibitor-aware notification resumed, meaning an inhibitor now is.
    Inhibited,
}

/// The state the protocol events update, and the queue of transitions.
///
/// Kept separate from the connection so the tracker's rules can be tested
/// without a compositor: [`IdleState::inhibited`] is the combination that matters
/// and is worth testing on its own.
#[derive(Debug, Default, Clone)]
pub struct IdleState {
    /// Whether input has gone quiet.
    input_idle: bool,
    /// Whether the inhibitor-aware notification is idle, which it cannot be while
    /// an inhibitor is held.
    inhibited_idle: bool,
    /// Transitions since the last drain, in order.
    pending: Vec<IdleEvent>,
}

impl IdleState {
    /// Whether input has gone quiet.
    #[must_use]
    pub fn input(&self) -> Input {
        if self.input_idle {
            Input::Idle
        } else {
            Input::Active
        }
    }

    /// Whether a client currently holds an idle inhibitor.
    ///
    /// Only meaningful while input is idle: the inhibitor-aware notification
    /// cannot fire while one is held, so "input idle but that one is not" is
    /// exactly the inhibited case.
    #[must_use]
    pub fn inhibited(&self) -> bool {
        self.input_idle && !self.inhibited_idle
    }

    /// Take the transitions recorded since the last call.
    pub fn drain(&mut self) -> Vec<IdleEvent> {
        std::mem::take(&mut self.pending)
    }
}

/// Everything the idle protocol needs from the client.
pub struct Idle {
    queue: EventQueue<IdleState>,
    state: IdleState,
    connection: Connection,
    /// Kept alive for as long as detection is wanted: destroying them stops the
    /// notifications.
    _notifier: ExtIdleNotifierV1,
    _input: ExtIdleNotificationV1,
    _inhibited: ExtIdleNotificationV1,
}

impl Idle {
    /// Connect and start watching for idleness of `timeout`.
    ///
    /// A separate connection from niri's IPC socket: this is an ordinary Wayland
    /// client, which is what the protocol requires.
    pub fn start(timeout: Duration) -> Result<Self, Error> {
        let connection =
            Connection::connect_to_env().map_err(|err| Error::Connect(err.to_string()))?;

        // A queue is needed before binding, and the registry round trip needs a
        // queue of the same type, so the same queue is reused throughout.
        let (globals, queue) = registry_queue_init::<IdleState>(&connection)
            .map_err(|err| Error::Protocol(err.to_string()))?;
        let handle = queue.handle();

        let notifier = bind_notifier(&globals, &handle)?;
        let seat = bind_seat(&globals, &handle)?;

        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let input = notifier.get_input_idle_notification(timeout_ms, &seat, &handle, InputMarker);
        let inhibited = notifier.get_idle_notification(timeout_ms, &seat, &handle, InhibitedMarker);

        // Flush so the compositor starts the timers now rather than at the next
        // dispatch.
        connection
            .flush()
            .map_err(|err| Error::Protocol(err.to_string()))?;

        Ok(Self {
            queue,
            state: IdleState::default(),
            connection,
            _notifier: notifier,
            _input: input,
            _inhibited: inhibited,
        })
    }

    /// The state as last dispatched.
    #[must_use]
    pub fn state(&self) -> &IdleState {
        &self.state
    }

    /// Read and dispatch whatever the compositor has sent, without blocking.
    ///
    /// Returns the transitions in order. The daemon calls this and acts on each,
    /// because "input just went quiet" and "an inhibitor appeared" need different
    /// handling even though both arrive as events.
    ///
    /// Reading and dispatching are two steps and both are required:
    /// `dispatch_pending` only handles events already sitting in the queue's
    /// buffer, so without a read first nothing ever arrives and the daemon would
    /// silently never learn that the user left.
    pub fn poll(&mut self) -> Result<Vec<IdleEvent>, Error> {
        // Flush our own requests before reading, so the compositor has them.
        self.queue
            .flush()
            .map_err(|err| Error::Protocol(err.to_string()))?;

        // `prepare_read` returns `None` when another thread holds the read guard,
        // in which case dispatching what is buffered is all that can be done.
        if let Some(guard) = self.queue.prepare_read()
            && let Err(err) = guard.read()
            && !is_would_block(&err)
        {
            return Err(Error::Protocol(err.to_string()));
        }

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|err| Error::Protocol(err.to_string()))?;
        Ok(self.state.drain())
    }

    /// Block until something arrives, then dispatch it.
    ///
    /// Used by the daemon's own loop when there is nothing else to do.
    pub fn block(&mut self) -> Result<Vec<IdleEvent>, Error> {
        self.queue
            .blocking_dispatch(&mut self.state)
            .map_err(|err| Error::Protocol(err.to_string()))?;
        Ok(self.state.drain())
    }

    /// The connection, for a caller that wants to put its fd in an event loop.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Whether an error is simply "nothing has arrived yet".
///
/// A non-blocking read that finds an empty socket reports this, and it is the
/// normal state of affairs in a poll loop rather than a failure.
fn is_would_block(error: &wayland_client::backend::WaylandError) -> bool {
    match error {
        wayland_client::backend::WaylandError::Io(err) => {
            err.kind() == std::io::ErrorKind::WouldBlock
        }
        _ => false,
    }
}

/// Bind the notifier, refusing a version too old for input-only notifications.
fn bind_notifier(
    globals: &GlobalList,
    handle: &QueueHandle<IdleState>,
) -> Result<ExtIdleNotifierV1, Error> {
    let mut found = None;
    let mut too_old = false;

    for global in globals.contents().clone_list() {
        if global.interface != NOTIFIER_INTERFACE {
            continue;
        }
        if global.version < REQUIRED_VERSION {
            too_old = true;
            continue;
        }
        // The second argument is a *version* range, not the global's name:
        // `GlobalList::bind` looks the interface up itself and binds the lower of
        // the advertised and the requested version.
        let bound = globals
            .bind::<ExtIdleNotifierV1, IdleState, ()>(
                handle,
                REQUIRED_VERSION..=REQUIRED_VERSION,
                (),
            )
            .map_err(|err| Error::Protocol(err.to_string()))?;
        found = Some(bound);
    }

    match (found, too_old) {
        (Some(notifier), _) => Ok(notifier),
        (None, true) => Err(Error::Unsupported),
        (None, false) => Err(Error::Unsupported),
    }
}

/// Bind the first seat.
///
/// One seat is all that is needed: the idle timeout is a property of the
/// compositor's view of input, and every seat it advertises shares it.
fn bind_seat(globals: &GlobalList, handle: &QueueHandle<IdleState>) -> Result<WlSeat, Error> {
    for global in globals.contents().clone_list() {
        if global.interface != SEAT_INTERFACE {
            continue;
        }
        // Version 1 is enough: this module only needs the seat's identity to
        // attach idle notifications to, never any of its input events.
        let bound = globals
            .bind::<WlSeat, IdleState, ()>(handle, 1..=9, ())
            .map_err(|err| Error::Protocol(err.to_string()))?;
        return Ok(bound);
    }
    Err(Error::NoSeat)
}

impl Dispatch<WlRegistry, GlobalListContents> for IdleState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for IdleState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for IdleState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, InputMarker> for IdleState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: NotificationEvent,
        _data: &InputMarker,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            NotificationEvent::Idled => {
                state.input_idle = true;
                state.pending.push(IdleEvent::InputIdle);
            }
            NotificationEvent::Resumed => {
                state.input_idle = false;
                state.pending.push(IdleEvent::InputActive);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, InhibitedMarker> for IdleState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: NotificationEvent,
        _data: &InhibitedMarker,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            NotificationEvent::Idled => {
                state.inhibited_idle = true;
                state.pending.push(IdleEvent::Uninhibited);
            }
            NotificationEvent::Resumed => {
                state.inhibited_idle = false;
                state.pending.push(IdleEvent::Inhibited);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> IdleState {
        IdleState::default()
    }

    #[test]
    fn active_input_is_active() {
        let mut state = state();
        assert_eq!(state.input(), Input::Active);
        assert!(!state.inhibited());
        assert!(state.drain().is_empty());
    }

    #[test]
    fn quiet_input_without_an_inhibitor_is_plain_idle() {
        let mut state = state();
        state.input_idle = true;
        state.inhibited_idle = true;
        assert_eq!(state.input(), Input::Idle);
        assert!(
            !state.inhibited(),
            "both notifications fired, so nothing is held"
        );
    }

    #[test]
    fn input_idle_while_inhibited_is_the_video_case() {
        let mut state = state();
        state.input_idle = true;
        state.inhibited_idle = false;
        assert_eq!(state.input(), Input::Idle);
        assert!(
            state.inhibited(),
            "a held inhibitor is exactly this combination"
        );
    }

    #[test]
    fn resumed_input_clears_the_inhibitor_reading() {
        let mut state = state();
        state.input_idle = true;
        state.input_idle = false;
        assert_eq!(state.input(), Input::Active);
        assert!(!state.inhibited());
    }

    #[test]
    fn events_are_reported_in_order_and_drained_once() {
        let mut state = state();
        state.pending.push(IdleEvent::InputIdle);
        state.pending.push(IdleEvent::Inhibited);
        assert_eq!(
            state.drain(),
            vec![IdleEvent::InputIdle, IdleEvent::Inhibited]
        );
        assert!(
            state.drain().is_empty(),
            "a second drain must not repeat them"
        );
    }
}
