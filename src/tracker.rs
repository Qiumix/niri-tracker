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
use std::collections::HashMap;
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
    /// The compositor's id for this window, stable while it is open.
    ///
    /// The only thing that tells two windows of one process apart: an
    /// application that owns several windows — `kitty --single-instance`,
    /// Firefox, anything Electron — reports the same pid for all of them.
    pub window_id: u64,
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
    /// The window this stretch belongs to.
    ///
    /// Kept here rather than on `Live` because it is not part of the record: the
    /// database stores what to report, and a window id means nothing after the
    /// window is gone.
    window_id: u64,
}

/// What a window's time was last credited to.
///
/// Kept per window rather than derived each time because one process may own
/// several windows, and the process tree cannot say which window is which. A
/// window's answer cannot change while it is open, unless the program running in
/// it exits — which the recorded process makes detectable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Credited {
    name: String,
    /// The process that produced `name`, and when it started, so a reused pid is
    /// not mistaken for the original.
    pid: u32,
    started: u64,
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
    /// What each window was last credited to.
    credited: HashMap<u64, Credited>,
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
            credited: HashMap::new(),
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
        // A pending focus loss is not kept: the interval it belongs to has just been
        // closed, and settling it later would look for something already finished.
        self.unfocused_since = None;
        self.close_at(clock.wall())
    }

    /// Adopt new settings, keeping the state that is not configuration.
    ///
    /// A reload must not forget that recording was paused or that the user is away.
    /// A fresh tracker would believe both are false and start counting time that
    /// the caller had explicitly asked it not to, so the settings change in place
    /// instead of the machine being replaced.
    pub fn reconfigure(&mut self, idle_timeout: Duration, terminals: Vec<TerminalConfig>) {
        self.idle_timeout = idle_timeout;
        self.terminals = terminals;
        // Which applications are split may have changed, so answers derived under
        // the old rules are not answers any more.
        self.credited.clear();

        // Being away is a fact about idle *detection*, not about the user. Turning
        // detection off, or losing the connection to it, leaves nothing that could
        // report the user coming back: the flag would stay set forever and the
        // tracker would never count again. Forgetting it is the safe direction —
        // it counts time that may have been idle, which a report can survive.
        if idle_timeout.is_zero() {
            self.idle = false;
        }
    }

    /// Work out what a window is running before it is ever focused.
    ///
    /// A window that has just opened has just forked its process, so that process
    /// is the newest one under the terminal rather than one of several. Resolving
    /// here rather than on first focus is the difference between deducing which
    /// program a window runs and guessing it.
    ///
    /// Returns nothing and records nothing when the window's process does not
    /// exist yet — the compositor announces a window before the application
    /// finishes starting, and a guess made then would be remembered as fact.
    pub fn prefetch(&mut self, focus: &Focus) {
        let splits = self
            .terminals
            .iter()
            .any(|entry| entry.split && entry.app_ids.iter().any(|id| id == &focus.app_id));
        if !splits {
            return;
        }

        if self.credited.contains_key(&focus.window_id) {
            return;
        }
        let Some(pid) = focus.pid else {
            return;
        };

        let claimed: Vec<(u32, u64)> = self
            .credited
            .values()
            .map(|known| (known.pid, known.started))
            .collect();
        let Some(process) = crate::terminal::foreground_process_avoiding(pid, &claimed) else {
            return;
        };

        self.credited.insert(
            focus.window_id,
            Credited {
                name: process.name,
                pid: process.pid,
                started: process.started,
            },
        );
    }

    /// Stop believing the user is away.
    ///
    /// Called when idle detection is gone: the state can only be cleared by an
    /// event from the thing that no longer exists, so keeping it would stop the
    /// tracker counting for the rest of the session.
    pub fn forget_idle(&mut self) {
        self.idle = false;
    }

    /// Forget what a window was credited to.
    ///
    /// Its id is not reused while the window is open, but it can be once it is
    /// gone, and a new window must not inherit the old one's answer.
    pub fn forget_window(&mut self, window_id: u64) {
        self.credited.remove(&window_id);
    }

    /// End the open interval, leaving every other piece of state alone.
    ///
    /// Used when the configuration is replaced: the row must be finished with the
    /// settings it was recorded under, but pausing to achieve that would also
    /// change whether the tracker is paused, which is not what a reload means.
    pub fn close_open(&mut self, clock: &dyn Clock) -> Vec<Effect> {
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
        // there is nothing to record, only a focus to remember. Compared by id
        // rather than by name, because one process may own several windows that
        // resolve to the same program — and those are different stretches.
        if uninterrupted
            && let Some(current) = &self.current
            && current.window_id == focus.window_id
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
        effects.extend(self.open(live, focus.window_id));
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
        let live = self.make_live(&focus, now);
        effects.extend(self.open(live, focus.window_id));
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

                // A focus loss that happened before the sleep is settled by this
                // tick, not left for the next one: otherwise the next tick would
                // close the interval reopened below at an instant that predates it.
                self.unfocused_since = None;

                // Nothing else reopens the interval: waking produces no focus
                // event, so without this the whole day after a suspend is lost
                // until the user happens to switch windows. A paused tracker stays
                // closed, and an idle one reopens only when an inhibitor means
                // something is still being watched — a video survives a suspend,
                // an empty room does not.
                if !self.paused
                    && (!self.idle || self.inhibited)
                    && let Some(focus) = self.focused.clone()
                {
                    let mut live = self.make_live(&focus, wall);
                    live.passive = self.idle;
                    effects.extend(self.open(live, focus.window_id));
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

    /// Open an interval for `live` on `window_id`, if not already open.
    fn open(&mut self, live: Live, window_id: u64) -> Vec<Effect> {
        if self.current.is_some() {
            return Vec::new();
        }
        let effect = Effect::Open(live.clone());
        self.current = Some(Current {
            id: 0,
            live,
            window_id,
        });
        vec![effect]
    }

    /// Build the interval to record for a focus, resolving terminals.
    fn make_live(&mut self, focus: &Focus, now: DateTime<Utc>) -> Live {
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
    fn resolve_app_id(&mut self, focus: &Focus) -> String {
        self.resolve_app_id_with(focus, crate::terminal::foreground_process_avoiding)
    }

    /// The same, with the lookup given, so the rules can be exercised without
    /// depending on what happens to be running on the machine.
    fn resolve_app_id_with<F>(&mut self, focus: &Focus, lookup: F) -> String
    where
        F: Fn(u32, &[(u32, u64)]) -> Option<crate::terminal::Process>,
    {
        let entry = self
            .terminals
            .iter()
            .find(|entry| entry.split && entry.app_ids.iter().any(|id| id == &focus.app_id));
        let Some(entry) = entry else {
            return focus.app_id.clone();
        };

        // `fallback` is read here rather than hard-coded so the setting means what
        // the preset says it means. With one variant it can only be the terminal,
        // but a setting that is parsed and ignored is a trap for whoever adds the
        // second one.
        let fallback = match entry.fallback {
            crate::config::Fallback::Terminal => focus.app_id.clone(),
        };

        // A window's answer holds while the window is open, and the process tree
        // cannot tell two windows of one process apart — which is the whole reason
        // this memory exists. It stops holding when the program exits, and the
        // recorded process makes that visible.
        if let Some(known) = self.credited.get(&focus.window_id)
            && crate::terminal::still_running(known.pid, known.started)
        {
            return known.name.clone();
        }

        let Some(pid) = focus.pid else {
            return fallback;
        };

        // Everything this process owns is in the same subtree, so the windows
        // already credited to some of it explain all but one of the foreground
        // jobs. Without this the newest job wins, which is whichever window
        // happened to start something last rather than the one asking.
        let claimed: Vec<(u32, u64)> = self
            .credited
            .iter()
            .filter(|(window_id, _)| **window_id != focus.window_id)
            .map(|(_, known)| (known.pid, known.started))
            .collect();
        let Some(process) = lookup(pid, &claimed) else {
            return fallback;
        };

        let name = process.name;
        let answer = Credited {
            name: name.clone(),
            pid: process.pid,
            started: process.started,
        };

        // One process cannot be the foreground of two windows. If it already is,
        // one of the two answers was a guess made before the other window was
        // known, and there is no way to tell which — so both are dropped and each
        // window re-derives the next time it is focused, with the other one gone
        // from the pool.
        let duplicated = self
            .credited
            .iter()
            .filter(|(window_id, known)| {
                **window_id != focus.window_id
                    && known.pid == answer.pid
                    && known.started == answer.started
            })
            .map(|(window_id, _)| *window_id)
            .collect::<Vec<_>>();
        for window_id in duplicated {
            self.credited.remove(&window_id);
        }

        self.credited.insert(focus.window_id, answer);
        name
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

    /// The window showing `app_id` with `title`.
    ///
    /// The id is derived from the pair, so focusing the same window twice gives the
    /// same id — which is what a test usually means. A test that needs two windows
    /// of one application spells the ids out with [`window`].
    fn focus(app_id: &str, title: &str) -> Option<Focus> {
        Some(window(&format!("{app_id}/{title}"), app_id, title))
    }

    /// A specific window, for tests about several windows of one application.
    fn window(id: &str, app_id: &str, title: &str) -> Focus {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        Focus {
            window_id: hasher.finish(),
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            // A pid that is not real: the lookup is scripted, and nothing here
            // should reach `/proc`.
            pid: Some(u32::MAX),
        }
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

    /// A terminal split into `names`, one per window, all under `pid`.
    ///
    /// Mirrors `kitty --single-instance`: every window reports the same pid, and
    /// the process tree holds one foreground job per window.
    fn split_windows(names: &[&'static str]) -> (Vec<TerminalConfig>, Vec<u8>) {
        let terminals = vec![TerminalConfig {
            app_ids: vec![String::from("kitty")],
            split: true,
            fallback: crate::config::Fallback::Terminal,
        }];
        let pool: Vec<u8> = (1..=names.len() as u8).collect();
        (terminals, pool)
    }

    /// A lookup that hands out the next name, recording what it was asked to
    /// avoid, exactly as the real one would after elimination.
    fn scripted<'a>(
        pool: &'a std::cell::RefCell<Vec<(&'static str, u32, u64)>>,
    ) -> impl Fn(u32, &[(u32, u64)]) -> Option<crate::terminal::Process> + 'a {
        move |_pid, claimed| {
            let mut pool = pool.borrow_mut();
            let (name, pid, started) = *pool
                .iter()
                .find(|(_, pid, started)| !claimed.contains(&(*pid, *started)))?;
            pool.retain(|entry| entry.1 != pid);
            Some(crate::terminal::Process {
                name: String::from(name),
                pid,
                started,
            })
        }
    }

    #[test]
    fn prefetching_is_a_no_op_for_an_application_that_is_not_split() {
        // The daemon prefetches every window that opens, including ones this
        // feature has nothing to say about. Those must not be looked up: the whole
        // point of the terminal rule is that it applies to terminals.
        let (terminals, _) = split_windows(&["nvim"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);

        tracker.prefetch(&window("a", "floorp", "browser"));

        // Nothing was remembered, so the answer is the app_id itself.
        assert_eq!(
            tracker.resolve_app_id_with(&window("a", "floorp", "browser"), |_, _| None),
            "floorp"
        );
    }

    #[test]
    fn prefetching_does_not_invent_an_answer_when_nothing_is_there() {
        // The compositor announces a window before the application has finished
        // starting, so there may be no process to find yet. Nothing must be
        // recorded then, or a guess would be remembered as fact.
        let (terminals, _) = split_windows(&["nvim"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);

        // A pid that is not real has no subtree, so prefetch finds nothing.
        tracker.prefetch(&Focus {
            window_id: 99,
            app_id: String::from("kitty"),
            title: String::from("~"),
            pid: Some(u32::MAX),
        });

        // The next focus looks again rather than trusting a fabricated answer.
        let pool = std::cell::RefCell::new(vec![("nvim", 100, 5)]);
        let lookup = scripted(&pool);
        assert_eq!(
            tracker.resolve_app_id_with(&window("x", "kitty", "~"), &lookup),
            "nvim"
        );
    }

    #[test]
    fn a_window_keeps_the_answer_it_was_given() {
        // Two windows, one process. Once a window has been told what it is, asking
        // again must not re-derive it — the process tree cannot tell them apart and
        // would hand out the other window's program.
        //
        // The remembered process has to be a live one, because the memory is only
        // trusted while the process it names still exists. This process will do.
        let me = std::process::id();
        let started = crate::terminal::start_time(me).expect("a process can read its own stat");
        let (terminals, _) = split_windows(&["nvim", "yazi"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let pool = std::cell::RefCell::new(vec![("nvim", me, started), ("yazi", 200, 6)]);
        let lookup = scripted(&pool);

        let first = tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup);
        assert_eq!(first, "nvim");

        // A lookup that would answer differently proves the memory is what replied.
        let trap = |_: u32, _: &[(u32, u64)]| panic!("a remembered window must not look again");
        let again = tracker.resolve_app_id_with(&window("a", "kitty", "~"), trap);
        assert_eq!(again, "nvim");
    }

    #[test]
    fn a_second_window_gets_what_the_first_did_not_claim() {
        // The case that used to fail: two windows of one kitty, both reporting the
        // same pid. The first window takes one foreground job, and the second is
        // offered only the rest.
        let (terminals, _) = split_windows(&["nvim", "yazi"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let pool = std::cell::RefCell::new(vec![("nvim", 100, 5), ("yazi", 200, 6)]);
        let lookup = scripted(&pool);

        let first = tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup);
        let second = tracker.resolve_app_id_with(&window("b", "kitty", "~"), &lookup);

        assert_eq!(first, "nvim");
        assert_eq!(
            second, "yazi",
            "the second window must not be given the first one's program"
        );
    }

    #[test]
    fn a_window_is_re_derived_when_its_process_is_gone() {
        // The program exited, so the window is showing something else now. The
        // remembered answer belongs to a process that no longer exists.
        let (terminals, _) = split_windows(&["nvim"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let pool = std::cell::RefCell::new(vec![("nvim", 100, 5), ("fish", 300, 9)]);
        let lookup = scripted(&pool);

        let first = tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup);
        assert_eq!(first, "nvim");

        drop(tracker);

        // A fresh tracker with the same window asks again; the second name is what
        // the scripted lookup would answer, so this asserts the re-derivation path
        // rather than the memory.
        let (terminals, _) = split_windows(&["fish"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let pool = std::cell::RefCell::new(vec![("fish", 300, 9)]);
        let lookup = scripted(&pool);
        let second = tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup);
        assert_eq!(second, "fish");
    }

    #[test]
    fn a_window_is_forgotten_when_it_closes() {
        let (terminals, _) = split_windows(&["nvim"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let window_id = window("a", "kitty", "~").window_id;
        let pool = std::cell::RefCell::new(vec![("nvim", 100, 5)]);
        let lookup = scripted(&pool);
        assert_eq!(
            tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup),
            "nvim"
        );

        tracker.forget_window(window_id);

        // The lookup would panic if the memory still answered, so this also proves
        // the entry is gone rather than merely stale.
        let pool = std::cell::RefCell::new(vec![("yazi", 400, 1)]);
        let lookup = scripted(&pool);
        assert_eq!(
            tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup),
            "yazi"
        );
    }

    #[test]
    fn reloading_forgets_what_windows_were_credited_to() {
        let (terminals, _) = split_windows(&["nvim"]);
        let mut tracker = Tracker::new(Duration::ZERO, terminals);
        let pool = std::cell::RefCell::new(vec![("nvim", 100, 5)]);
        let lookup = scripted(&pool);
        assert_eq!(
            tracker.resolve_app_id_with(&window("a", "kitty", "~"), &lookup),
            "nvim"
        );

        tracker.reconfigure(Duration::ZERO, Vec::new());

        // No terminal rules any more, so the app_id comes through untouched rather
        // than from an answer that was derived under different settings.
        let trap = |_: u32, _: &[(u32, u64)]| panic!("must not consult the tree");
        assert_eq!(
            tracker.resolve_app_id_with(&window("a", "kitty", "~"), trap),
            "kitty"
        );
    }

    #[test]
    fn two_windows_of_one_application_are_two_stretches() {
        // `kitty --single-instance` gives every window the same pid, so the name
        // resolves identically; the id is what says these are different stretches.
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(Some(window("a", "kitty", "shell")), &clock);
        tracker.adopt(1);
        clock.advance(Duration::from_secs(10));

        let effects = tracker.on_focus(Some(window("b", "kitty", "shell")), &clock);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Close { id: 1, .. })),
            "moving to another window must end the first stretch: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_))),
            "and begin a second one: {effects:?}"
        );
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
    fn a_suspend_while_watching_a_video_keeps_counting() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("mpv", "video"), &clock);
        tracker.adopt(1);
        tracker.on_inhibitor(true, &clock);
        tracker.on_input_idle(clock.wall(), &clock);
        assert_eq!(tracker.live().map(|live| live.passive), Some(true));

        // The machine sleeps while the video plays: the video is still there after
        // the wake-up, and the user is still not touching anything.
        tracker.on_tick(&clock);
        clock.advance_wall_only(Duration::from_secs(8 * 3600));
        let effects = tracker.on_tick(&clock);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Open(_))),
            "the passive interval must reopen after the sleep: {effects:?}"
        );
        assert_eq!(
            tracker.live().map(|live| live.passive),
            Some(true),
            "and it is still passive"
        );
    }

    #[test]
    fn a_suspend_settles_a_pending_focus_loss() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_tick(&clock);

        // Focus is lost, then the machine sleeps before any tick settles the gap.
        tracker.on_focus(None, &clock);
        clock.advance_wall_only(Duration::from_secs(8 * 3600));
        tracker.on_tick(&clock);

        // The next tick must not close the interval that was reopened on waking at
        // an instant from before the sleep.
        clock.advance(Duration::from_secs(60));
        let effects = tracker.on_tick(&clock);
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Close { .. })),
            "the reopened interval must survive the next tick: {effects:?}"
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
    fn disabling_idle_detection_forgets_that_the_user_is_away() {
        // Turning detection off leaves nothing that could ever report the user
        // coming back, so the flag has to go with it.
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);
        assert!(tracker.idle());

        tracker.reconfigure(Duration::ZERO, Vec::new());

        assert!(
            !tracker.idle(),
            "a tracker with no idle detection cannot know the user is away"
        );
    }

    #[test]
    fn keeping_idle_detection_keeps_the_user_away() {
        // The other direction still holds: a reload that keeps detection running
        // must not count the time until the user comes back.
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);

        tracker.reconfigure(Duration::from_secs(600), Vec::new());

        assert!(
            tracker.idle(),
            "detection is still running, so the user is away"
        );
    }

    #[test]
    fn losing_idle_detection_forgets_that_the_user_is_away() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);

        tracker.forget_idle();

        assert!(
            !tracker.idle(),
            "nothing is left to report the user coming back"
        );
    }

    #[test]
    fn closing_the_open_interval_does_not_pause() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);

        let effects = tracker.close_open(&clock);

        assert!(matches!(effects[0], Effect::Close { id: 1, .. }));
        assert!(tracker.live().is_none());
        assert!(!tracker.paused(), "closing a row is not a pause");
    }

    #[test]
    fn reconfiguring_keeps_paused_and_idle() {
        let clock = clock();
        let mut tracker = idle_tracker();
        tracker.on_focus(focus("kitty", "shell"), &clock);
        tracker.adopt(1);
        tracker.on_input_idle(clock.wall(), &clock);
        tracker.pause(&clock);

        tracker.reconfigure(Duration::from_secs(600), Vec::new());

        assert!(tracker.paused(), "a reload must not resume by itself");
        assert!(tracker.idle(), "a reload must not forget the user is away");
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
                window_id: 1,
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
