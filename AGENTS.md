# Working on niri-tracker

Notes for an agent (or a person) changing this code. Read `docs/design.md` before
changing behaviour: most of the non-obvious decisions are there, with the
alternatives that were ruled out.

## Build and check

```sh
cargo test                     # 100 tests, no external services
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

All three must be clean before a commit. CI runs exactly these.

The crate uses edition 2024 and needs a recent stable toolchain. Tabs, not spaces
(`rustfmt.toml` sets `hard_tabs`).

## Layout

| Path | What lives there |
|---|---|
| `src/config/` | Preset merged with the user file. Everything is resolved here; nothing downstream sees an `Option`. |
| `src/db/` | The `session` table. One writer: the daemon. |
| `src/tracker.rs` | **The rules.** What counts as screen time, in one state machine. |
| `src/compositor/niri.rs` | niri IPC → focus changes. |
| `src/compositor/idle.rs` | `ext-idle-notify-v1` → idleness and inhibitors. |
| `src/terminal.rs` | `/proc` → the foreground process of a terminal. |
| `src/report.rs` | Grouping, aliases, title normalisation. |
| `src/ipc/` | The control socket. Wire format in `protocol.rs`. |
| `src/daemon.rs` | Wires everything together on one thread. |
| `src/main.rs` | The command line. |
| `preset/config.toml` | The defaults, compiled in with `include_str!`. |
| `packaging/` | Two PKGBUILDs and the systemd unit. Not built by cargo. |

## Rules this codebase actually follows

**No `unwrap_or` for configuration defaults.** Defaults live in
`preset/config.toml`, which is a complete document, so after merging there is
nothing left to default. If you find yourself writing `unwrap_or(5)` for a
setting, the setting belongs in the preset instead.

**The state machine does no I/O.** `src/tracker.rs` takes a `Clock` and returns
`Effect` values; `src/daemon.rs` applies them. This is what makes every timing
rule testable in microseconds. Do not reach for `Utc::now()` inside the tracker.

**Tests are the specification.** Every rule in `docs/design.md` has a test named
after it in `src/tracker.rs`. If you change a rule, change the test and the
document in the same commit — a rule that exists only in code will be undone by
the next person who finds it surprising.

**Errors that a user can cause are fatal.** A malformed configuration stops the
program. A missing one does not. The distinction is whether the user believes they
configured something.

**Comments explain why.** The code says what it does. Where a comment repeats the
code, delete it. Where a decision looks wrong until you know the reason — the
two-second focus grace, the 30-second suspend threshold, trimming to
`now - timeout` — that reason belongs next to it.

## Changing the configuration format

1. Add the field to `src/config/model.rs`, as `Option<T>`.
2. Set its default in `preset/config.toml`, with a comment explaining the choice.
3. Merge it in `Config::merge` and resolve it in `Config::resolve`.
4. Add a test in `src/config/mod.rs` that the preset parses and resolves.
5. Regenerate the schema if a packaged copy exists: `cargo run -- schema`.

`deny_unknown_fields` is on: a misspelled key is an error, not a silent
no-op. Keep it that way.

## Changing the wire protocol

See `docs/protocol.md`. A new verb is additive and does not bump
`INFO_VERSION`. A changed field name is not: bump it.

## Things that are easy to get wrong

- **`/proc/<pid>/stat`** — `comm` may contain spaces and `)`, so fields are
  counted from the *last* `)`. `tpgid` and `pgrp` are the terminal's foreground
  process group; `tty_nr != 0` means it has a terminal at all.
- **Idle events arrive one timeout late.** Cut the interval at `now - timeout`,
  never at `now`.
- **Suspend detection compares two clocks.** `Instant` does not advance while
  suspended; `Utc::now()` does. Comparing the monotonic elapsed time against the
  tick length can never detect a suspend, because during one the monotonic clock
  is exactly what stops moving.
- **The two idle notifications mean opposite things.** `get_idle_notification`
  is inhibited by `zwp_idle_inhibitor_v1`; `get_input_idle_notification` is not.
  Input idle *without* the first one firing is the video case.
- **`GlobalList::bind` takes a version range, not a global name.**
- **Reading and dispatching Wayland events are two steps.** `dispatch_pending`
  only handles what is already buffered; without a `prepare_read` first, nothing
  ever arrives and the daemon silently never learns anything.

## Testing against a real session

Unit tests cover every rule, but three things need a live compositor:

```sh
# Idle detection. Use a timeout long enough to outlast ordinary device jitter.
mkdir -p /tmp/nt-test && printf '[idle]\ntimeout = "5m"\n' > /tmp/nt-test/config.toml
NIRI_TRACKER_CONFIG_DIR=/tmp/nt-test ./target/debug/niri-tracker daemon
# Then leave the keyboard alone for five minutes and watch for
# "niri-tracker: idle: InputIdle" on stderr.
```

```sh
# Crash recovery: kill -9 leaves a row open, the next start closes it and prints
# "closed N dangling interval(s)".
```

Inhibitors (a video player holding the screen awake) are the one path that has no
test yet. The protocol comparison is in `src/compositor/idle.rs`; the tracker's
reaction is `Tracker::on_inhibitor`.

## Commits

Imperative subject, a body that says why rather than what. The bodies in
`git log` are the model: they state the problem, the decision, and what was
rejected.

## Releasing

The version lives in `Cargo.toml` and nowhere else — there is no `version.go`
equivalent. Tagging is what triggers the release workflow.

```sh
# 1. Bump the version and commit.
# 2. Tag and push.
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
```

The workflow then builds both targets, attaches them to the GitHub release, and
the AUR `-bin` package picks them up. `-git` follows `main` and needs no action.

For crates.io, `cargo publish --registry crates-io` after the tag.
