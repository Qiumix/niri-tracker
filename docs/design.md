# Design

Why niri-tracker is shaped the way it is. The README says what it does; this says
what was ruled out and why, so a future change does not have to rediscover it.

## Intervals, not samples

The obvious design records a row every second saying which window was focused,
then sums rows to answer a query. It is simple, and it fails on every question
worth asking.

**Idle.** Sampling records what was focused, not whether anyone was looking. An
idle machine left on overnight reports eight hours of screen time. The fix — "it
was idle, so those samples do not count" — requires knowing *when* idleness began,
which a per-second sample cannot tell you: it only knows when it noticed.

**Videos.** Idle detection alone cuts a video off after the timeout, because
watching one involves no input at all. The fix requires a second signal, and
combining two signals at query time over a pile of samples is where the logic
becomes impossible to test.

**Suspend.** Wall-clock time jumps forward across a suspend. With samples, the
jump is invisible: the rows simply stop, and the gap is never counted either way.
With intervals, the gap is a subtraction that has to be gotten right — which is
harder but at least visible.

**Volume.** Five samples a second is 432,000 rows a day. The Go version this
replaces had an entire module whose job was to compact them into sessions every
ten minutes. Intervals make that module unnecessary: a day is a few hundred rows,
and a report is `SUM(end - start)`.

The cost is that every rule about *when* to start and stop needs writing down,
and that is what `src/tracker.rs` is: one state machine, no I/O, every rule
testable without a compositor or a clock.

## Two connections to the compositor

This looks like duplication and is not.

niri's IPC socket carries window events: focus changed, window opened, closed.
That is enough for everything except idleness, and niri's IPC carries **no input
timestamps at all**. `Window::focus_timestamp` exists and looks promising, but its
documentation is explicit: it is a debounced most-recently-used marker for
alt-tab, which only updates after a delay and only for window switching. Using it
to measure idleness would be wrong in a way that is hard to notice.

So idleness comes from `ext-idle-notify-v1`, over an ordinary Wayland connection.
That is the only supported way for a client to ask a compositor "has the user
stopped touching things", and niri supports it.

## Two idle notifications, not one

`ext-idle-notify-v1` version 2 offers two ways to create a notification, and the
difference is the entire mechanism:

| Request | Inhibited by `zwp_idle_inhibitor_v1` |
|---|---|
| `get_idle_notification` | Yes — an inhibitor stops it firing at all |
| `get_input_idle_notification` | No — fires strictly on real input going quiet |

Creating both and comparing them distinguishes the two cases that matter:

- Input idle **and** the inhibitor-aware one also idle → the user left. Stop
  counting.
- Input idle **but** the inhibitor-aware one still awake → someone is holding the
  screen awake. Keep counting, and mark the time passive.

A single notification cannot tell these apart. Using only the inhibitor-aware one
means an unattended machine with a video paused on screen counts forever. Using
only the input-only one means every video is cut off after the timeout.

This is also why the protocol version matters: version 1 lacks
`get_input_idle_notification`, so the comparison is impossible. The daemon refuses
version 1 rather than silently degrading.

## Stretches are clipped, and may cross midnight

A stretch is recorded as one row, and a row can span midnight. Reports therefore
select by overlap — `end > from AND start < to` — and clip each stretch to the
requested window before crediting it. A stretch running from 23:30 to 00:30 counts
half an hour towards each day, and asking for the second day finds it.

Grouping by day or hour splits the clipped stretch at local boundaries for the
same reason: a two-hour stretch must not appear as a single row under the hour it
happened to begin in.

The `day` column still exists, but only as the local date a stretch began on; it
is not a filter.

## Cutting back to when input stopped

The `idled` event arrives **one timeout after** input actually stopped. It has to:
the compositor cannot know that input has stopped until the timeout has elapsed.

So the naive handling — "the interval ends now" — over-counts by the full timeout
on every single idle event. With a five-minute timeout, that is five extra minutes
of screen time every time someone walks away, which at a few absences per hour is
a 20% error.

Instead the interval is trimmed back to `now - timeout`, and the record is
rewritten. It costs one `UPDATE` and makes the number correct.

## Focus gaps are provisional

Pressing the overview, opening a layer-shell popup, or taking a screenshot drops
window focus for a moment. Ending the interval each time would turn an hour of
reading into hundreds of rows, and the report would be unreadable.

So a focus loss is **provisional**: the interval stays open, and if focus returns
to the same window within two seconds the whole thing is treated as never having
happened. If it does not, the next tick closes the interval *at the moment focus
was lost*, not at the tick.

`None` still stops counting immediately in the sense that nothing new is opened
while nothing is focused. The two cases — lock screen for an hour, overview for
half a second — are told apart by whether focus comes back.

The comparison is by **window id**, not by name. A name cannot answer "same
window?": one process may own several windows that resolve to the same program,
and two stretches under that name are still two stretches.

The compositor's window id does answer it, and niri keeps it stable for as long as
the window is open.

The same comparison makes a focus event for the window already being timed a
no-op. niri sends a window list on every window opening or closing anywhere on
the session, and the daemon re-queries the focus in response; without this, one
interval would be replaced by another dozens of times an hour while nothing
actually changed on screen.

## Suspends are detected by the two clocks disagreeing

`Instant` reads `CLOCK_MONOTONIC`, which **does not advance while suspended**.
`Utc::now()` reads the wall clock, which does. So a wall-clock jump much larger
than the monotonic one is a suspend, and the interval ends at the last tick plus
however long the machine was genuinely awake.

This is worth spelling out because the obvious test — comparing the monotonic
elapsed time against the nominal tick length — cannot work: during a suspend, the
monotonic clock is precisely what does *not* move, so the naive check sees nothing
unusual and extends the interval across the entire sleep.

The threshold is 30 s of disagreement, which is well above ordinary drift between
the two clocks and well below any real suspend.

Waking produces no focus event, so the tick that detects the suspend is also what
starts the next interval; otherwise everything after a sleep would be lost until
the user happened to switch windows. A paused tracker stays closed, and an idle
one reopens only if an inhibitor is held — a video survives a suspend, an empty
room does not — marking the new interval passive, which is what it is.

## Terminals are resolved through /proc

A terminal reports the same `app_id` for every window and every tab, so an hour in
nvim is credited to `kitty`. The interesting name is one level down: whatever is
in the foreground of the terminal's controlling pty.

The kernel already knows this. `/proc/<pid>/stat` carries `tpgid`, the process
group that owns the terminal, so a descendant of the terminal whose own process
group equals that value *is* the foreground job. No cooperation from the terminal,
no dependence on the shell setting a title, no terminal-specific code.

Only the terminal's own subtree is ever interesting, and kernels built with
`CONFIG_PROC_CHILDREN` — every distribution kernel — publish each thread's direct
children in `/proc/<pid>/task/<tid>/children`, so the subtree is walked downward
in a dozen file reads rather than reconstructed from a scan of every process.
`children` belongs to a thread, not a process, so all of them are read and merged.
A kernel without the feature falls back to the whole-table scan; walking reports
"unsupported" rather than "nothing below", so the two answers stay distinct.

Three details matter:

- `comm` in `/proc/<pid>/stat` can contain spaces and parentheses, so fields are
  counted from the last `)` rather than from the start of the line.
- The process name comes from `/proc/<pid>/cmdline`, not `comm`, because the
  kernel truncates `comm` to 15 bytes.
- With several tabs open, several foreground jobs exist and the compositor does
  not say which tab is visible. The newest process wins, on the grounds that
  starting a job is what makes a tab interesting.

When the lookup fails, the terminal's own `app_id` is used. Crediting time to a
guess would be worse than crediting it to the window that is unmistakably on
screen.

## The daemon answers queries

Reports and `status` go through the daemon's socket rather than reading the SQLite
file directly. Two reasons:

**The present.** The interval being recorded right now is in memory, not yet
written. A client reading the file directly would get a report that silently
omits the last minute, which is exactly the minute a user is most likely to be
checking.

**One rulebook.** Aliases, title normalisation and grouping live in one place. A
direct reader would have to reimplement them, and the two would drift.

The cost is that a report needs a running daemon. That is a deliberate trade: being
told "no daemon is running" is better than silently getting a number that is
quietly wrong.

## Configuration is fatal on error

Most programs fall back to defaults when a configuration file is malformed. This
one does not.

The reasoning is specific to a tracker: a misspelled `time_out` that silently does
nothing means the user believes idle detection is on and it is not. They will not
notice for days, and by then the data is wrong in a way that cannot be repaired.
Failing loudly at startup is unpleasant once; failing silently is unpleasant
forever.

Absence of the file is not an error — that is the normal case on a fresh install.

The same reasoning applies to the location itself: with neither `XDG_CONFIG_HOME`
nor `HOME` set, the directory functions used to build a **relative** path, which
anchors the database to whatever directory the process started in. They report an
error naming the variable they consulted instead, because writing a database
somewhere nobody chose is the same failure one level down.

## Arrays append to the preset

Scalars merge cleanly: a user value overrides a preset value, and "absent" is
just `None`. Arrays have no such state.

The preset ships a list of common terminals. If arrays replaced wholesale, adding
one terminal to the config would silently drop all the built-in ones, which is the
opposite of what a preset is for. So arrays **append** by default, and
`merge_preset_arrays = false` is the explicit escape hatch for taking full
control.

## Pausing is not persisted

`pause` stops recording until `resume`, and a daemon restart resumes recording
rather than staying paused.

A tracker that stays paused after a restart fails in the hardest way to notice:
the reports are simply short, and nothing says why. Pausing is a momentary action
and should not survive the process that it was asked of.

## One daemon per session, decided by a lock

Two daemons recording the same session would count every second twice, and
nothing about the resulting report would look wrong. Looking for a live instance
and refusing if one is found sounds like enough, but it is a read followed by a
decision: two daemons started at the same moment both find none.

So an exclusive `flock` on `<display>.lock` decides. The kernel releases it when
the process dies, including on a panic, which the pid-named files cannot promise,
and the holder writes its pid into the file so a refusal can still name it.

The lock file is never unlinked. Unlinking would let a second process create a
fresh file and lock that instead, and both would believe they hold the slot.

Each instance removes **its own** socket and metadata when it exits, never the
directory: the runtime directory is shared by every session on the machine, and
several daemons are normal — two niri logins, a nested compositor.

## Files

| File | Responsibility |
|---|---|
| `src/config/` | Preset merged with the user file; everything resolved, nothing optional |
| `src/db/` | The `session` table and nothing else |
| `src/tracker.rs` | The state machine: every rule about what counts |
| `src/compositor/niri.rs` | niri IPC → focus changes |
| `src/compositor/idle.rs` | `ext-idle-notify-v1` → idleness and inhibitors |
| `src/terminal.rs` | `/proc` → foreground process of a terminal |
| `src/report.rs` | Grouping, aliases, title normalisation |
| `src/ipc/` | The control socket: one request, one line, one reply |
| `src/daemon.rs` | Wires the above together on one thread |
| `src/main.rs` | The command line |

The state machine has no I/O and no clock of its own: it takes a `Clock` trait, so
tests drive time by hand and every rule above is asserted without waiting.
