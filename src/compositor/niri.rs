//! Talking to niri.
//!
//! Two connections, for two different jobs:
//!
//! - The IPC socket carries events: which window is focused, when it changed,
//!   when a window opened or closed. One request (`EventStream`) turns it into a
//!   stream, after which niri sends events and reads nothing more, so queries go
//!   over a second connection opened on demand.
//! - The Wayland connection carries idle notifications. niri does not expose
//!   input idleness over IPC at all, and the `ext-idle-notify-v1` protocol is the
//!   only supported way to ask.
//!
//! Both are optional in different ways. Without niri there is nothing to record;
//! without idle detection a daemon still records everything, just without
//! knowing when the user walked away.

use chrono::{DateTime, Utc};
use niri_ipc::{Event, Request, Response};

use crate::tracker::Focus;

/// Why the compositor connection failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The IPC socket is not reachable, or has gone away.
    #[error("cannot reach niri: {0}")]
    Ipc(#[from] std::io::Error),

    /// niri refused the event stream request.
    #[error("niri refused the event stream: {0}")]
    Refused(String),

    /// An event could not be decoded. Usually a version mismatch.
    #[error("cannot decode an event from niri: {0}")]
    Decode(#[from] serde_json::Error),
}

/// A change worth reacting to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    /// The focused window changed. `None` means nothing is focused.
    Focus(Option<Focus>),
    /// The window list changed. Only the focus inside it is interesting, but the
    /// event must be handled because niri sends focus changes for existing
    /// windows this way when the focused window was already open.
    Windows,
}

/// Turn one niri event into a change, or nothing.
///
/// Only three events matter. `WindowsChanged` carries the full list and is used
/// to find the focused window; `WindowFocusChanged` names it directly. Everything
/// else — layout updates, keyboard layouts, screencasts — is noise here, and new
/// variants niri adds later must not stop the daemon, so unknown events are
/// ignored rather than treated as errors.
#[must_use]
pub fn interpret(event: &Event, known: &mut Vec<Window>) -> Option<Watch> {
    match event {
        Event::WindowsChanged { windows } => {
            *known = windows.iter().map(Window::from).collect();
            Some(Watch::Windows)
        }
        Event::WindowOpenedOrChanged { window } => {
            let updated = Window::from(window);
            match known.iter_mut().find(|existing| existing.id == updated.id) {
                Some(existing) => *existing = updated,
                None => known.push(updated),
            }
            Some(Watch::Windows)
        }
        Event::WindowClosed { id } => {
            known.retain(|window| window.id != *id);
            Some(Watch::Windows)
        }
        Event::WindowFocusChanged { id } => {
            let focus = id.and_then(|id| {
                known
                    .iter()
                    .find(|window| window.id == id)
                    .map(Window::to_focus)
            });
            Some(Watch::Focus(focus))
        }
        _ => None,
    }
}

/// The focused window out of a list of known windows.
#[must_use]
pub fn focused_in(known: &[Window]) -> Option<Focus> {
    known
        .iter()
        .find(|window| window.focused)
        .map(Window::to_focus)
}

/// The parts of a niri window that matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// niri's own id, stable while the window is open.
    pub id: u64,
    /// The application, when it set one.
    pub app_id: Option<String>,
    /// Its title, when it set one.
    pub title: Option<String>,
    /// The pid that created the window, when niri knows it.
    pub pid: Option<i32>,
    /// Whether it currently has focus.
    pub focused: bool,
}

impl From<&niri_ipc::Window> for Window {
    fn from(window: &niri_ipc::Window) -> Self {
        Self {
            id: window.id,
            app_id: window.app_id.clone(),
            title: window.title.clone(),
            pid: window.pid,
            focused: window.is_focused,
        }
    }
}

impl Window {
    /// This window as the tracker sees it.
    ///
    /// A window with no app_id is not something a report can name, so a
    /// placeholder is used rather than an empty row that looks like a bug.
    fn to_focus(&self) -> Focus {
        Focus {
            app_id: self
                .app_id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| String::from("(unknown)")),
            title: self.title.clone().unwrap_or_default(),
            pid: self.pid.and_then(|pid| u32::try_from(pid).ok()),
        }
    }
}

/// Open an event stream and hand every event to `on_event`.
///
/// Blocks forever: it is meant to run on its own thread, because the stream is a
/// blocking socket and the daemon has other work to do. Returns when the
/// connection fails, which the caller treats as "reconnect".
/// `on_connected` runs once the stream is established and before the first event.
///
/// The caller needs that moment to tell "connected, then dropped" from "could not
/// connect": only the first deserves a backoff reset, and the function cannot
/// report it any other way without returning while the stream is still live.
pub fn run_event_stream<F, R>(mut on_event: F, mut on_connected: R) -> Result<(), Error>
where
    F: FnMut(Event),
    R: FnMut(),
{
    let mut socket = niri_ipc::socket::Socket::connect()?;
    match socket.send(Request::EventStream)? {
        Ok(Response::Handled) => {}
        Ok(other) => return Err(Error::Refused(format!("unexpected reply {other:?}"))),
        Err(message) => return Err(Error::Refused(message)),
    }

    on_connected();

    let mut read = socket.read_events();
    loop {
        match read() {
            Ok(event) => on_event(event),
            Err(err) => return Err(Error::Ipc(err)),
        }
    }
}

/// Ask niri for the focused window over a fresh connection.
///
/// Used after a reconnect, because the event stream starts with a full snapshot
/// but a `WindowFocusChanged` for the window that already had focus is only sent
/// once focus moves.
pub fn query_focused() -> Result<Option<Focus>, Error> {
    let mut socket = niri_ipc::socket::Socket::connect()?;
    match socket.send(Request::FocusedWindow)? {
        Ok(Response::FocusedWindow(window)) => {
            Ok(window.map(|window| Window::from(&window).to_focus()))
        }
        Ok(other) => Err(Error::Refused(format!("unexpected reply {other:?}"))),
        Err(message) => Err(Error::Refused(message)),
    }
}

/// The instant an event happened, for callers that need one.
///
/// niri does not timestamp events, so the best available answer is "now". Kept
/// as a function so the intent is explicit rather than an unexplained
/// `Utc::now()` at a call site.
#[must_use]
pub fn event_time() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, app_id: &str, focused: bool) -> niri_ipc::Window {
        niri_ipc::Window {
            id,
            title: Some(format!("title {id}")),
            app_id: Some(app_id.to_owned()),
            pid: Some(1000),
            workspace_id: Some(1),
            is_focused: focused,
            is_floating: false,
            is_urgent: false,
            layout: niri_ipc::WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (0.0, 0.0),
                window_size: (0, 0),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn a_full_window_list_replaces_what_is_known() {
        let mut known = Vec::new();
        let event = Event::WindowsChanged {
            windows: vec![window(1, "kitty", true), window(2, "firefox", false)],
        };
        let watch = interpret(&event, &mut known);
        assert_eq!(watch, Some(Watch::Windows));
        assert_eq!(known.len(), 2);
        assert_eq!(
            focused_in(&known).map(|focus| focus.app_id),
            Some(String::from("kitty"))
        );
    }

    #[test]
    fn an_opened_window_is_added() {
        let mut known = vec![Window::from(&window(1, "kitty", false))];
        let event = Event::WindowOpenedOrChanged {
            window: window(2, "firefox", false),
        };
        let _ = interpret(&event, &mut known);
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn a_changed_window_is_replaced_not_duplicated() {
        let mut known = vec![Window::from(&window(1, "kitty", false))];
        let event = Event::WindowOpenedOrChanged {
            window: window(1, "kitty", true),
        };
        let _ = interpret(&event, &mut known);
        assert_eq!(known.len(), 1);
        assert!(known[0].focused);
    }

    #[test]
    fn a_closed_window_is_forgotten() {
        let mut known = vec![
            Window::from(&window(1, "kitty", true)),
            Window::from(&window(2, "firefox", false)),
        ];
        let event = Event::WindowClosed { id: 1 };
        let _ = interpret(&event, &mut known);
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].id, 2);
    }

    #[test]
    fn a_focus_change_names_the_window() {
        let mut known = vec![
            Window::from(&window(1, "kitty", false)),
            Window::from(&window(2, "firefox", true)),
        ];
        let event = Event::WindowFocusChanged { id: Some(2) };
        let watch = interpret(&event, &mut known);
        match watch {
            Some(Watch::Focus(Some(focus))) => assert_eq!(focus.app_id, "firefox"),
            other => panic!("expected a focus, got {other:?}"),
        }
    }

    #[test]
    fn losing_focus_is_reported_as_none() {
        let mut known = Vec::new();
        let event = Event::WindowFocusChanged { id: None };
        assert_eq!(interpret(&event, &mut known), Some(Watch::Focus(None)));
    }

    #[test]
    fn a_focus_change_for_an_unknown_window_is_none() {
        let mut known = Vec::new();
        let event = Event::WindowFocusChanged { id: Some(99) };
        assert_eq!(interpret(&event, &mut known), Some(Watch::Focus(None)));
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let mut known = Vec::new();
        let event = Event::OverviewOpenedOrClosed { is_open: true };
        assert_eq!(interpret(&event, &mut known), None);
    }

    #[test]
    fn a_window_without_an_app_id_gets_a_placeholder() {
        let mut known = Vec::new();
        let mut raw = window(1, "", false);
        raw.app_id = None;
        let _ = interpret(&Event::WindowOpenedOrChanged { window: raw }, &mut known);
        assert_eq!(known[0].to_focus().app_id, "(unknown)");
    }

    #[test]
    fn a_negative_pid_becomes_none() {
        let mut known = Vec::new();
        let mut raw = window(1, "kitty", false);
        raw.pid = Some(-1);
        let _ = interpret(&Event::WindowOpenedOrChanged { window: raw }, &mut known);
        assert_eq!(known[0].to_focus().pid, None);
    }
}
