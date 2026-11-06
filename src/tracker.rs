//! The state machine that turns compositor events into intervals.
//!
//! This is the part that decides what counts. It is driven by events rather than
//! polling, and it keeps at most one interval open.
//!
//! The rules, in one place:
//!
//! - A focused window starts an interval; switching windows, closing one, or
//!   losing focus ends it.
//! - Nothing focused means nothing counted. A locked screen must not be screen
//!   time. The overview and layer-shell popups drop focus for a moment, so a gap
//!   shorter than [`FOCUS_GAP_GRACE`] that returns to the same window is
//!   forgiven rather than shredding an interval into confetti.
//! - When input has been idle for the configured timeout, the interval is cut
//!   back to *when input stopped*, not to when the event arrived: the event is
//!   delivered one timeout later, so using it directly would over-count by that
//!   much every single time.
//! - An idle inhibitor, such as a video player holding the screen awake, means
//!   input idleness alone is not evidence of absence. The interval keeps running
//!   and is flagged passive, so a report can leave it out.
//! - A suspend is not screen time. The monotonic clock stops while suspended, so
//!   a wall-clock jump larger than the monotonic one is the tell, and the
//!   interval ends where the wake-up gap began.
//!
//! The machine never touches storage. It describes changes as [`Effect`] values
//! and the daemon applies them, which is what lets every rule above be tested
//! without a file on disk or a compositor running.

use std::cell::Cell;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::config::TerminalConfig;
use crate::db::Live;

/// How long a gap in focus is forgiven before the interval really ends.
pub const FOCUS_GAP_GRACE: Duration = Duration::from_secs(2);

/// How far the wall clock may outrun the monotonic clock before the difference
/// is put down to a suspend.
pub const SUSPEND_THRESHOLD: Duration = Duration::from_secs(30);

/// How often the daemon checkpoints and looks for a suspend.
pub const TICK: Duration = Duration::from_secs(60);

/// A clock the machine can be driven by, so tests need not wait.
pub trait Clock {
    /// The wall clock, which jumps forward across a suspend.
    fn wall(&self) -> DateTime<Utc>;
    /// A monotonic reading, which does not advance while suspended.
    fn monotonic(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn wall(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn monotonic(&self) -> Instant {
        Instant::now()
    }
}

/// A window the compositor says is focused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// The application, as the compositor reports it.
    pub app_id: String,
    /// Its window title.
    pub title: String,
    /// The pid that created the window, when the compositor knows it. Needed to
    /// look inside a terminal.
    pub pid: Option<u32>,
}

/// What to do to storage as a result of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Open a new interval. The daemon inserts it and reports the new id back
    /// through [`Tracker::adopt`].
    Open(Live),
    /// Move the open interval's end forward.
    Extend {
        /// The row to update.
        id: i64,
        /// Its new end.
        end: DateTime<Utc>,
    },
    /// Finish the open interval.
    Close {
        /// The row to update.
        id: i64,
        /// Where it ends.
        end: DateTime<Utc>,
    },
    /// Mark the open interval as passive, i.e. covered by an idle inhibitor.
    ///
    /// This is a storage effect and not just a flag on the in-memory copy: a
    /// report reads the column, so keeping it only in memory would make
    /// `exclude_passive` silently do nothing.
    MarkPassive {
        /// The row to update.
        id: i64,
    },
}

/// The interval currently open, and the row holding it.
#[derive(Debug, Clone)]
struct Current {
    /// Zero until storage assigns one.
    id: i64,
    /// What is being recorded.
    live: Live,
}

/// The tracking state machine.
pub struct Tracker {
    current: Option<Current>,
    focused: Option<Focus>,
    idle_timeout: Duration,
    idle: bool,
    inhibited: bool,
    paused: bool,
    /// The previous tick, as both a monotonic and a wall reading, so a suspend can
    /// be told apart from ordinary passage of time.
    last_tick: Option<(Instant, DateTime<Utc>)>,
    /// When focus was lost provisionally, so a brief blink can be forgiven.
    unfocused_since: Option<DateTime<Utc>>,
    terminals: Vec<TerminalConfig>,
}

impl Tracker {
    /// Build a tracker from the resolved configuration.
    #[must_use]
    pub fn new(idle_timeout: Duration, terminals: Vec<TerminalConfig>) -> Self {
        Self {
            current: None,
            focused: None,
            idle_timeout,
            idle: false,
            inhibited: false,
            paused: false,
            last_tick: None,
            unfocused_since: None,
            terminals,
        }
    }

    /// Whether recording is paused.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Whether input is currently idle.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.idle
    }

    /// Whether an idle inhibitor is active.
    #[must_use]
    pub fn inhibited(&self) -> bool {
        self.inhibited
    }

    /// The interval being recorded, for `status` and for a report's live row.
    #[must_use]
    pub fn live(&self) -> Option<&Live> {
        self.current.as_ref().map(|current| &current.live)
    }

    /// Record the id storage assigned to the interval just opened.
    pub fn adopt(&mut self, id: i64) {
        if let Some(current) = &mut self.current {
            current.id = id;
        }
    }

    /// Stop recording, closing whatever is open.
    pub fn pause(&mut self, clock: &dyn Clock) -> Vec<Effect> {
        if self.paused {
            return Vec::new();
        }
        self.paused = true;
        // The idle flag is deliberately kept: after resuming, the tracker must
        // still know the user is away, or it counts the time until input returns.
        self.close_at(clock.wall())
    }

    /// Resume recording.
    ///
    /// Nothing is opened here: the daemon asks the compositor for the current
    /// focus afterwards, because after a pause the tracker no longer knows what is
    /// on screen.
    pub fn resume(&mut self) -> Vec<Effect> {
        self.paused = false;
        Vec::new()
    }

    /// The focused window changed.
    ///
    /// A focus of `None` is treated as *provisional*: the interval keeps running
    /// for [`FOCUS_GAP_GRACE`] because the overview and layer-shell popups drop
    /// focus for a moment, and shredding an interval every time the overview
    /// blinks would make reports unreadable. [`Tracker::on_tick`] and any real
    /// focus event settle it.
    pub fn on_focus(&mut self, focus: Option<Focus>, clock: &dyn Clock) -> Vec<Effect> {
        let now = clock.wall();

        let Some(focus) = focus else {
            // Nothing focused. Stop counting at once: a locked screen must not be
            // screen time, and it is better to under-count a two-second overview
            // than to count an unattended machine. The interval stays open so the
            // near-term outcome can be repaired if focus comes straight back.
            self.focused = None;
            if self.current.is_some() {
                self.unfocused_since = Some(now);
            }
            return Vec::new();
        };

        let returning = self.unfocused_since.take();

        if self.paused || self.idle {
            // Remember where to start when the reason to wait goes away.
            self.focused = Some(focus);
            return Vec::new();
        }

        // The name the interval holds, which for a split terminal is the program
        // running inside it rather than the terminal's own app_id. Comparing the
        // raw app_id here would never match, so every window-list event would end
        // the interval and start another one for the same window.
        let resolved = self.resolve_app_id(&focus);
        let grace = chrono::Duration::from_std(FOCUS_GAP_GRACE).unwrap_or_default();
        let uninterrupted = returning.is_none_or(|since| now - since < grace);

        // The same window is still the one being timed, and nothing interrupted it:
        // there is nothing to record, only a focus to remember.
        if uninterrupted
            && let Some(current) = &self.current
            && current.live.app_id == resolved
            && current.live.title == focus.title
            && !current.live.passive
        {
            self.focused = Some(focus);
            return Vec::new();
        }

        // A window that came back after the grace has passed ended the interval when
        // it lost focus, not now: the time in between was not spent on it.
        let mut effects = self.close_at(returning.unwrap_or(now));
        let live = Live {
            app_id: resolved,
            title: focus.title.clone(),
            passive: false,
            start: now,
        };
        effects.extend(self.open(live));
        self.focused = Some(focus);
        effects
    }

    /// Input has been idle for the configured timeout.
    ///
    /// `idled_at` is when input actually stopped, which is one timeout before the
    /// event arrives.
    pub fn on_input_idle(&mut self, idled_at: DateTime<Utc>, clock: &dyn Clock) -> Vec<Effect> {
        if self.idle_timeout.is_zero() || self.idle {
            return Vec::new();
        }
        self.idle = true;

        // While paused nothing is recorded, but the idleness still has to be
        // remembered, or resuming would count time away as time spent.
        if self.paused {
            return Vec::new();
        }

        // An inhibitor means someone is watching or presenting. Keep counting, but
        // mark the row so a report can leave it out.
        if self.inhibited {
            let Some(current) = &mut self.current else {
                return Vec::new();
            };
            current.live.passive = true;
            let id = current.id;
            let mut effects = vec![Effect::MarkPassive { id }];
            effects.extend(self.extend_to(clock.wall()));
            return effects;
        }

        // End the interval at the moment input stopped, which is when it really
        // stopped being a useful measurement.
        let Some(current) = self.current.take() else {
            return Vec::new();
        };
        vec![Effect::Close {
            id: current.id,
            end: idled_at.max(current.live.start),
        }]
    }

    /// Input started again after being idle.
    pub fn on_input_active(&mut self, clock: &dyn Clock) -> Vec<Effect> {
        if !self.idle {
            return Vec::new();
        }
        self.idle = false;

        if self.paused {
            return Vec::new();
        }

        let Some(focus) = self.focused.clone() else {
            return Vec::new();
        };
        let now = clock.wall();
        // A row carries one `passive` flag, so an interval that was passive must
        // not continue as an active one: close it at the moment input returned and
        // start a fresh interval with the flag clear.
        let mut effects = self.close_at(now);
        effects.extend(self.open(self.make_live(&focus, now)));
        effects
    }

    /// The idle inhibitor state changed.
    pub fn on_inhibitor(&mut self, active: bool, clock: &dyn Clock) -> Vec<Effect> {
        let was = self.inhibited;
        self.inhibited = active;
        if was == active {
            return Vec::new();
        }

        // An inhibitor appearing while input is idle means watching, not leaving.
        if active && self.idle && !self.paused {
            let Some(current) = &mut self.current else {
                return Vec::new();
            };
            current.live.passive = true;
            let id = current.id;
            let mut effects = vec![Effect::MarkPassive { id }];
            effects.extend(self.extend_to(clock.wall()));
            return effects;
        }

        // It went away while input is still idle, so the watching stopped.
        if !active && self.idle {
            return self.close_at(clock.wall());
        }

        Vec::new()
    }

    /// Checkpoint the open interval, and notice a suspend.
    pub fn on_tick(&mut self, clock: &dyn Clock) -> Vec<Effect> {
        let monotonic = clock.monotonic();
        let wall = clock.wall();

        // A suspend is detected by the two clocks disagreeing: the wall clock ran
        // while the monotonic one did not. Comparing monotonic progress against the
        // nominal tick length cannot see a suspend at all, because during one the
        // monotonic clock is exactly what does not move.
        let previous = self.last_tick.replace((monotonic, wall));
        if let Some((previous_monotonic, previous_wall)) = previous {
            let awake = monotonic.saturating_duration_since(previous_monotonic);
            let wall_elapsed = wall - previous_wall;
            let wall_elapsed = wall_elapsed.to_std().unwrap_or_default();

            if wall_elapsed > awake.saturating_add(SUSPEND_THRESHOLD) {
                // The interval ended when the machine went to sleep: the last tick
                // plus however long it was genuinely awake afterwards. Counting the
                // sleep would add hours of "screen time" to the day.
                let cut = previous_wall + chrono::Duration::from_std(awake).unwrap_or_default();
                let mut effects = self.close_at(cut);
                // Nothing else reopens the interval: waking produces no focus
                // event, so without this the whole day after a suspend is lost
                // until the user happens to switch windows.
                if !self.paused
                    && !self.idle
                    && let Some(focus) = self.focused.clone()
                {
                    effects.extend(self.open(self.make_live(&focus, wall)));
                }
                return effects;
            }
        }

        // A provisional focus loss is settled once the grace has really elapsed.
        // Settling it on the first tick after any loss would cut the interval short
        // when a tick happens to land inside the grace period.
        if let Some(lost_at) = self.unfocused_since {
            let grace = chrono::Duration::from_std(FOCUS_GAP_GRACE).unwrap_or_default();
            if wall - lost_at >= grace {
                self.unfocused_since = None;
                return self.close_at(lost_at);
            }
            return Vec::new();
        }

        if self.paused {
            return Vec::new();
        }

        // Extended even while idle: an inhibitor keeps the interval open then, and
        // without a checkpoint a crash would lose the whole passive stretch.
        self.extend_to(wall)
    }

    /// End the open interval at `now`.
    fn close_at(&mut self, now: DateTime<Utc>) -> Vec<Effect> {
        match self.current.take() {
            Some(current) => vec![Effect::Close {
                id: current.id,
                end: now.max(current.live.start),
            }],
            None => Vec::new(),
        }
    }

    /// Move the open interval's end to `now`.
    fn extend_to(&mut self, now: DateTime<Utc>) -> Vec<Effect> {
        match &self.current {
            Some(current) => vec![Effect::Extend {
                id: current.id,
                end: now.max(current.live.start),
            }],
            None => Vec::new(),
        }
    }

    /// Open an interval for `live`, if not already open.
    fn open(&mut self, live: Live) -> Vec<Effect> {
        if self.current.is_some() {
            return Vec::new();
        }
        let effect = Effect::Open(live.clone());
        self.current = Some(Current { id: 0, live });
        vec![effect]
    }

    /// Build the interval to record for a focus, resolving terminals.
    fn make_live(&self, focus: &Focus, now: DateTime<Utc>) -> Live {
        Live {
            app_id: self.resolve_app_id(focus),
            title: focus.title.clone(),
            passive: false,
            start: now,
        }
    }

    /// Replace a terminal's app_id with the program running inside it.
    ///
    /// Falls back to the terminal's own app_id whenever the foreground process
    /// cannot be read, because crediting time to a guess would be worse than
    /// crediting it to the terminal that is unmistakably on screen.
    fn resolve_app_id(&self, focus: &Focus) -> String {
        let splits = self
            .terminals
            .iter()
            .any(|entry| entry.split && entry.app_ids.iter().any(|id| id == &focus.app_id));
        if !splits {
            return focus.app_id.clone();
        }
        let Some(pid) = focus.pid else {
            return focus.app_id.clone();
        };
        crate::terminal::foreground_process_name(pid).unwrap_or_else(|| focus.app_id.clone())
    }
}

/// A clock tests can drive by hand.
#[derive(Debug)]
pub struct ManualClock {
    wall: Cell<DateTime<Utc>>,
    monotonic: Cell<Instant>,
}

impl ManualClock {
    /// Start at a fixed instant.
    #[must_use]
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            wall: Cell::new(start),
            monotonic: Cell::new(Instant::now()),
        }
    }

    /// Move both clocks forward.
    pub fn advance(&self, by: Duration) {
        self.wall
            .set(self.wall.get() + chrono::Duration::from_std(by).unwrap_or_default());
        self.monotonic.set(self.monotonic.get() + by);
    }

    /// Move only the wall clock, as a suspend does.
    pub fn advance_wall_only(&self, by: Duration) {
        self.wall
            .set(self.wall.get() + chrono::Duration::from_std(by).unwrap_or_default());
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> DateTime<Utc> {
        self.wall.get()
    }

    fn monotonic(&self) -> Instant {
        self.monotonic.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn clock() -> ManualClock {
        ManualClock::new(Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 0).unwrap())
    }

    fn focus(app_id: &str, title: &str) -> Option<Focus> {
        Some(Focus {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            pid: None,
        })
    }

    fn idle_tracker() -> Tracker {
        Tracker::new(Duration::from_secs(300), Vec::new())
    }

    #[test]
    fn focusing_opens_an_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::Open(_)));
        assert_eq!(
            tracker.live().map(|live| live.app_id.as_str()),
            Some("kitty")
        );
    }

    #[test]
    fn focusing_the_same_window_again_changes_nothing() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        clock.advance(Duration::from_secs(5));

        // The window list changed and the daemon re-queried the same window, as it
        // does on every window open or close anywhere on the session.
        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);

        assert!(
            effects.is_empty(),
            "the same window must not split the interval: {effects:?}"
        );
        assert_eq!(
            tracker.live().map(|live| live.app_id.as_str()),
            Some("kitty")
        );
    }

    #[test]
    fn returning_to_the_same_window_after_the_grace_starts_a_new_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        clock.advance(Duration::from_secs(30));

        tracker.on_focus(None, &clock);
        let lost_at = clock.wall();
        clock.advance(Duration::from_secs(10));

        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);

        // The old interval ends where focus was lost, so the time in between is not
        // counted against it, and the new one starts now.
        assert!(
            effects.contains(&Effect::Close {
                id: 1,
                end: lost_at
            }),
            "closed where focus was lost: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_)))
        );
    }

    #[test]
    fn switching_windows_closes_and_opens() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        clock.advance(Duration::from_secs(10));

        let effects = tracker.on_focus(focus("firefox", "mail"), &clock);
        assert_eq!(effects.len(), 2);
        assert!(matches!(effects[0], Effect::Close { id: 1, .. }));
        assert!(matches!(effects[1], Effect::Open(_)));
    }

    #[test]
    fn losing_focus_stops_counting() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        clock.advance(Duration::from_secs(10));

        // Losing focus is provisional: the interval stays open for a moment so a
        // blinking overview cannot split it.
        tracker.on_focus(None, &clock);
        clock.advance(FOCUS_GAP_GRACE + Duration::from_secs(1));

        // The next tick settles it, ending the interval when focus was lost, not
        // when the tick ran.
        let lost_at = clock.wall()
            - chrono::Duration::from_std(FOCUS_GAP_GRACE + Duration::from_secs(1)).unwrap();
        let effects = tracker.on_tick(&clock);
        assert!(matches!(effects[0], Effect::Close { id: 1, .. }));
        match &effects[0] {
            Effect::Close { end, .. } => {
                assert_eq!(*end, lost_at, "the interval ends when focus left")
            }
            other => panic!("expected a close, got {other:?}"),
        }
        assert!(tracker.live().is_none());
    }

    #[test]
    fn a_brief_gap_keeps_the_same_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        // The overview opened and closed within the grace period.
        tracker.on_focus(None, &clock);
        clock.advance(Duration::from_millis(500));
        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);

        assert!(
            effects.is_empty(),
            "a brief blink must not split the interval: {effects:?}"
        );
        assert_eq!(
            tracker.live().map(|live| live.app_id.as_str()),
            Some("kitty")
        );
    }

    #[test]
    fn a_long_gap_does_not_restore_the_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        tracker.on_focus(None, &clock);
        clock.advance(Duration::from_secs(5));
        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_)))
        );
    }

    #[test]
    fn idle_cuts_back_to_when_input_stopped() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        // Five minutes pass with no input; the event arrives now, but input
        // stopped five minutes ago.
        clock.advance(Duration::from_secs(300));
        let idled_at = clock.wall() - chrono::Duration::seconds(300);
        let effects = tracker.on_input_idle(idled_at, &clock);

        let close = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Close { end, .. } => Some(*end),
                _ => None,
            })
            .expect("closes");
        assert_eq!(close, idled_at, "the interval must end when input stopped");
        assert!(tracker.live().is_none());
    }

    #[test]
    fn idle_with_an_inhibitor_keeps_counting_and_is_passive() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);

        clock.advance(Duration::from_secs(300));
        let idled_at = clock.wall() - chrono::Duration::seconds(300);
        let effects = tracker.on_input_idle(idled_at, &clock);

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Close { .. })),
            "an inhibited session must keep running: {effects:?}"
        );
        assert_eq!(tracker.live().map(|live| live.passive), Some(true));
    }

    #[test]
    fn an_inhibitor_disappearing_while_idle_stops_counting() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);
        clock.advance(Duration::from_secs(300));
        tracker.on_input_idle(clock.wall() - chrono::Duration::seconds(300), &clock);

        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_inhibitor(false, &clock);
        assert!(matches!(effects[0], Effect::Close { .. }));
    }

    #[test]
    fn a_zero_timeout_disables_idle_detection() {
        let clock = clock();
        let mut tracker = Tracker::new(Duration::ZERO, Vec::new());
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        let effects = tracker.on_input_idle(clock.wall(), &clock);
        assert!(effects.is_empty());
        assert!(tracker.live().is_some());
    }

    #[test]
    fn input_becoming_active_again_reopens_an_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);

        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_input_active(&clock);
        assert!(matches!(effects[0], Effect::Open(_)));
    }

    #[test]
    fn a_suspend_does_not_count_as_screen_time() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        // A normal tick, then the machine sleeps for eight hours.
        tracker.on_tick(&clock);
        clock.advance(Duration::from_secs(60));
        tracker.on_tick(&clock);

        let before_sleep = clock.wall();
        clock.advance_wall_only(Duration::from_secs(8 * 3600));

        let effects = tracker.on_tick(&clock);
        let close = effects.iter().find_map(|effect| match effect {
            Effect::Close { end, .. } => Some(*end),
            _ => None,
        });
        let end = close.expect("the interval must be closed at the suspend");
        assert!(
            end < before_sleep + chrono::Duration::seconds(120),
            "the sleep must not be counted: ended {end}, woke at {}",
            clock.wall()
        );
    }

    #[test]
    fn an_ordinary_tick_extends_the_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_tick(&clock);
        assert!(matches!(effects[0], Effect::Extend { id: 1, .. }));
    }

    #[test]
    fn pausing_closes_the_interval_and_stops_counting() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        let effects = tracker.pause(&clock);
        assert!(matches!(effects[0], Effect::Close { id: 1, .. }));
        assert!(tracker.paused());
        assert!(tracker.live().is_none());

        // A focus event while paused must not open anything.
        let effects = tracker.on_focus(focus("firefox", "mail"), &clock);
        assert!(effects.is_empty());
    }

    #[test]
    fn resuming_waits_for_a_focus_event() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.pause(&clock);

        assert!(tracker.resume().is_empty());
        assert!(!tracker.paused());

        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);
        assert!(matches!(effects[0], Effect::Open(_)));
    }

    #[test]
    fn idle_keeps_the_time_until_input_stopped() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        // Ten minutes of work, then five minutes of nothing. The event arrives
        // now, at 10:05, but input stopped at 10:00.
        clock.advance(Duration::from_secs(600));
        let idled_at = clock.wall();
        clock.advance(Duration::from_secs(300));
        let effects = tracker.on_input_idle(idled_at, &clock);

        // The interval must end where input stopped, and it must still cover the
        // ten minutes before that.
        assert_eq!(
            effects,
            vec![Effect::Close {
                id: 1,
                end: idled_at
            }]
        );
    }

    #[test]
    fn a_suspend_reopens_the_interval_on_waking() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_tick(&clock);
        clock.advance(Duration::from_secs(60));
        tracker.on_tick(&clock);

        // Eight hours of sleep: the wall clock moves, the monotonic one does not.
        clock.advance_wall_only(Duration::from_secs(8 * 3600));
        let effects = tracker.on_tick(&clock);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Close { .. })),
            "the interval must end at the sleep: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_))),
            "a new interval must start on waking, or the day is lost: {effects:?}"
        );
        assert!(tracker.live().is_some());
    }

    #[test]
    fn pausing_while_idle_does_not_make_the_user_look_active() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);
        assert!(tracker.idle());

        tracker.pause(&clock);
        assert!(tracker.idle(), "idleness must survive a pause");
        tracker.resume();

        // Coming back must not count the time away: nothing opens until input
        // actually returns.
        let effects = tracker.on_focus(focus("kitty", "shell"), &clock);
        assert!(effects.is_empty(), "must wait for input: {effects:?}");
        assert!(tracker.live().is_none());
    }

    #[test]
    fn returning_after_a_passive_stretch_starts_a_clean_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);
        clock.advance(Duration::from_secs(300));
        tracker.on_input_idle(clock.wall() - chrono::Duration::seconds(300), &clock);
        assert_eq!(tracker.live().map(|live| live.passive), Some(true));

        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_input_active(&clock);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Close { .. })),
            "the passive stretch must be closed: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_))),
            "the active stretch must be a new row: {effects:?}"
        );
        assert_eq!(
            tracker.live().map(|live| live.passive),
            Some(false),
            "a row cannot be both passive and active"
        );
    }

    #[test]
    fn becoming_passive_is_written_to_storage() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);
        clock.advance(Duration::from_secs(300));

        let effects = tracker.on_input_idle(clock.wall() - chrono::Duration::seconds(300), &clock);
        assert!(
            effects.contains(&Effect::MarkPassive { id: 1 }),
            "exclude_passive reads the column, so the flag has to reach storage: {effects:?}"
        );
    }

    #[test]
    fn a_tick_inside_the_grace_period_does_not_end_the_interval() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        // Focus blinks away, and a tick lands one second later, still inside the
        // two-second grace.
        tracker.on_focus(None, &clock);
        clock.advance(Duration::from_secs(1));
        let effects = tracker.on_tick(&clock);
        assert!(
            effects.is_empty(),
            "the grace must still apply: {effects:?}"
        );
        assert!(tracker.live().is_some());

        // Once the grace has passed, the next tick settles it.
        clock.advance(Duration::from_secs(2));
        let effects = tracker.on_tick(&clock);
        assert!(matches!(effects[0], Effect::Close { .. }));
    }

    #[test]
    fn a_tick_while_inhibited_still_checkpoints() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);
        tracker.on_input_idle(clock.wall(), &clock);

        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_tick(&clock);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Extend { .. })),
            "a passive stretch needs checkpoints too, or a crash loses all of it: {effects:?}"
        );
    }

    #[test]
    fn a_terminal_without_a_readable_pid_keeps_its_own_name() {
        let clock = clock();
        let terminals = vec![crate::config::TerminalConfig {
            app_ids: vec![String::from("kitty")],
            split: true,
            fallback: crate::config::Fallback::Terminal,
        }];
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        // A pid that cannot exist, so the lookup fails and falls back.
        let effects = tracker.on_focus(
            Some(Focus {
                app_id: String::from("kitty"),
                title: String::from("x"),
                pid: Some(u32::MAX),
            }),
            &clock,
        );
        match &effects[0] {
            Effect::Open(live) => assert_eq!(live.app_id, "kitty"),
            other => panic!("expected an open, got {other:?}"),
        }
    }
}
