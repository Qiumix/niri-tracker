# niri-tracker

Screen time tracking for the [niri](https://github.com/YaLTeR/niri) Wayland
compositor.

It watches which window is focused, records how long each one holds it, and
reports the totals per application, per window title, or per day and hour. When
you stop touching the keyboard and mouse, it stops counting — unless something
is holding the screen awake to play a video, in which case it keeps counting and
marks the time as passive.

```text
$ niri-tracker report
QQ                      37m 15s
floorp                  20m 49s
fish                    4m 41s
nvim                    2m 47s
yazi                    36s
niri-tracker            22s
top.imsyy.splayer_next  17s
xargs                   9s
cargo                   4s
less                    4s
glow                    3s
fzf                     2s
rustc                   2s
file                    1s
sh                      1s

Total                   1h 7m 13s
```

## Why not just sample the active window?

The obvious design is to poll the active window every second and count the
samples. That approach cannot answer the questions people actually ask:

- **Was I at the keyboard?** Sampling tells you what window was focused, not
  whether anyone was looking at it. A tracker that counts an idle machine
  reports eight hours of screen time for a night's sleep.
- **Was that a video?** While a video plays, the keyboard is untouched for an
  hour, so idle detection would cut it off after five minutes. The screen is
  very much being used.
- **Did the machine sleep?** Wall-clock time jumps forward across a suspend, and
  naive subtraction turns a night of sleep into a night of work.

So this records **intervals** rather than samples: one row covers a stretch of
time spent on one window, and every rule above is applied when the stretch
begins and ends. A day is a few hundred rows instead of a few hundred thousand,
and a report is a range query.

## Requirements

- Linux with the niri compositor (26.04 or later).
- A Wayland session. Idle detection uses `ext-idle-notify-v1`, which niri has
  supported since 25.05.

Nothing else: the binary is statically linked against its own SQLite and needs no
system libraries beyond glibc.

## Install

### Arch Linux (AUR)

```sh
# Prebuilt, from the release workflow.
yay -S niri-tracker-bin

# Or build from the tip of main.
yay -S niri-tracker-git
```

### Cargo

```sh
cargo install niri-tracker
```

### cargo-binstall

```sh
cargo binstall niri-tracker
```

## Run

Start the daemon in your niri session. The simplest way is niri's own
`spawn-at-startup`, which gives it the right environment automatically:

```kdl
spawn-at-startup "niri-tracker" "daemon"
```

If you prefer systemd, the packaged unit handles it:

```sh
systemctl --user enable --now niri-tracker.service
```

> The unit is `PartOf=graphical-session.target` and needs `WAYLAND_DISPLAY` and
> `NIRI_SOCKET` in the user manager's environment. `niri --session` imports both,
> so this works without further setup when niri starts the session.

## Use

```sh
niri-tracker report                    # today, per application
niri-tracker report --from 2026-11-01  # since the first, inclusive
niri-tracker report --by day           # per day
niri-tracker report --by hour          # per hour of the day
niri-tracker report --json             # for scripts

niri-tracker details --app kitty       # window titles of one application

niri-tracker status                    # what the daemon is doing now
niri-tracker pause                     # stop recording until `resume`
niri-tracker resume
niri-tracker reload                    # re-read the configuration
niri-tracker quit
```

`--to` is **inclusive**: `--to 2026-11-05` covers all of that day, not up to its
midnight.

Report totals always cover everything counted; `--limit` only hides rows.

### Shell completion

```sh
niri-tracker completions fish > ~/.config/fish/completions/niri-tracker.fish
niri-tracker completions bash > /usr/share/bash-completion/completions/niri-tracker
niri-tracker completions zsh  > /usr/share/zsh/site-functions/_niri-tracker
```

## Configuration

Everything has a default, so a configuration file is optional. To see the
defaults with their comments:

```sh
niri-tracker config init          # writes $XDG_CONFIG_HOME/niri-tracker/config.toml
niri-tracker config check         # parse it and report problems
```

A user file only needs the keys it changes; anything absent keeps its default.
**A malformed file is fatal** rather than silently falling back to defaults: a
tracker that appears to honour a five-minute idle timeout while actually running
with none collects days of wrong data before anyone notices.

### Idle detection

```toml
[idle]
# Stop counting after this much time without keyboard or mouse input.
# "0s" disables idle detection entirely, which is the default: every second the
# machine is awake counts.
timeout = "5m"
```

When input goes quiet, the interval is cut back to **when input stopped**, not to
when the notification arrived — the notification is delivered one timeout later,
so using it directly would over-count by that much every single time.

A video player holding `zwp_idle_inhibitor_v1` keeps counting through the idle
period and marks the time passive:

```sh
niri-tracker report --exclude-passive   # leave out video time
```

### Terminals

A terminal reports one `app_id` for every window, so time in nvim would be
credited to `kitty`. These entries credit the foreground process instead, found
through `/proc`, which works for any terminal and needs no shell configuration:

```toml
[[terminal]]
app_ids = ["kitty", "alacritty", "foot", "wezterm", "ghostty"]
split = true
```

When the foreground process cannot be read — a permission problem, a race with
the process exiting — the time is credited to the terminal itself, because a
guess would be worse than the window that is unmistakably on screen.

### Aliases

Merge several applications into one name. Applied when a report is generated,
never to the stored rows, so it is always reversible:

```toml
[[alias]]
name = "Browser"
app_ids = ["firefox", "floorp"]
titles = ["GitHub", "Documentation"]   # optional: split by title substring
unmatched = "keep"                     # or "other", to fold the rest in
```

Arrays from this file are **appended** to the built-in defaults, so adding one
terminal does not silently discard the others. Set `merge_preset_arrays = false`
to replace them wholesale.

### Storage

```toml
[storage]
# Delete sessions older than this. "0s" keeps everything, which is the default.
retention = "365d"
```

### Validation

`niri-tracker schema` prints a JSON Schema for the file. `config init` writes a
`#:schema` line pointing at it so [taplo](https://taplo.tamasfe.dev/) can check
the file in your editor.

## Where data lives

| What | Default | Override |
|---|---|---|
| Database | `$XDG_DATA_HOME/niri-tracker/track.db` | `NIRI_TRACKER_DATA_DIR` |
| Configuration | `$XDG_CONFIG_HOME/niri-tracker/config.toml` | `NIRI_TRACKER_CONFIG_DIR` |
| Control socket | `$XDG_RUNTIME_DIR/niri-tracker/<pid>.sock` | `NIRI_TRACKER_RUNTIME_DIR` |
| Session lock | `$XDG_RUNTIME_DIR/niri-tracker/<display>.lock` | `NIRI_TRACKER_RUNTIME_DIR` |

One daemon per Wayland session; starting a second one in the same session is
refused by an exclusive lock on that session's `.lock` file. The kernel drops the
lock when the process ends — including a crash — and each instance removes only
its own socket and metadata when it exits, because two sessions on one machine
are normal (two niri logins, a nested compositor).

The lock file stays behind: removing it would let a second daemon create a fresh
one and believe the session is free.

If neither `XDG_CONFIG_HOME` nor `HOME` is set, the daemon refuses to guess rather
than resolving to a path relative to its working directory.

The database is plain SQLite, written by the daemon alone:

```sql
SELECT app_id, SUM(end - start) FROM session GROUP BY app_id ORDER BY 2 DESC;
```

## How it decides

| Situation | What is recorded |
|---|---|
| Focused window changes | The old interval ends, a new one begins |
| Nothing focused (lock screen, overview) | Nothing; a gap under 2 s is forgiven, since the overview blinks focus away |
| Input idle past the timeout | The interval ends **when input stopped** |
| Idle, but an inhibitor is held | Keeps counting, flagged passive |
| Machine suspends | The interval ends where the sleep began; a new one begins on waking, unless paused or idle — idle reopens only with an inhibitor, as passive |
| Daemon killed | The next start closes the dangling interval at its last checkpoint |
| `pause` | Nothing recorded; not persisted, so a restart resumes recording |

A note on that last row: pausing is deliberately not remembered. A daemon that
stays paused after a restart is the hardest kind of failure to notice — days
later, the reports are simply short and nothing says why.

## See also

- [docs/design.md](docs/design.md) — why the pieces are shaped this way.
- [docs/protocol.md](docs/protocol.md) — the control socket, for scripting.

## License

MIT.
