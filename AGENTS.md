# Working on niri-tracker

Notes for an agent (or a person) changing this code. Read `docs/design.md` before
changing behaviour: most of the non-obvious decisions are there, with the
alternatives that were ruled out.

## Build and check

```sh
cargo test                     # 154 tests, no external services
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Documents too: TOML via taplo, Markdown via rumdl.
taplo fmt --check Cargo.toml preset/config.toml taplo.toml rumdl.toml
rumdl check .
```

All of these must be clean before a commit. CI runs exactly these.

`taplo` needs its file list given explicitly: it has no exclude option and would
otherwise reformat the copy of this repository that `cargo package` leaves under
`target/`.

The crate uses edition 2024 and needs a recent stable toolchain. Formatting is
whatever default `cargo fmt` produces; there is no `rustfmt.toml`.

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
| `packaging/` | Two PKGBUILDs, their `.SRCINFO` templates, the systemd unit. Not built by cargo. |

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

**Logging goes through `tracing`.** `info` for what the daemon is doing, `warn`
for something that went wrong but did not stop it, `error` for an interval that
was not written. Nothing is logged from a loop that runs per event without a
reason, and an event that fires routinely is not worth its own line.

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
  process group; `tty_nr != 0` means it has a terminal at all. `starttime` is the
  twenty-second field overall, seventeen after the closing parenthesis.
- **A pid does not identify a process.** Pids are reused, so a remembered pid can
  belong to something else by the time it is checked. Pair it with `starttime`;
  `terminal::still_running` does.
- **The tracker remembers what each window was credited to, and the memory is
  load-bearing.** One process may own several windows, so the process tree cannot
  say which window is asking; elimination against what other windows claimed is
  what picks the right one. Do not "simplify" the memory away, and release it with
  `forget_window` when a window closes.
- **Idle events arrive one timeout late.** Cut the interval at `now - timeout`,
  never at `now`.
- **Suspend detection compares two clocks.** `Instant` does not advance while
  suspended; `Utc::now()` does. Comparing the monotonic elapsed time against the
  tick length can never detect a suspend, because during one the monotonic clock
  is exactly what stops moving.
- **The two idle notifications mean opposite things.** `get_idle_notification`
  is inhibited by `zwp_idle_inhibitor_v1`; `get_input_idle_notification` is not.
  Input idle *without* the first one firing is the video case.
- **A window is identified by its id, never by its name.** One process can own
  several windows — `kitty --single-instance`, Firefox, anything Electron — and
  they all report the same pid and often the same app_id. `Focus::window_id` is
  the only thing that tells them apart; comparing names merges stretches that
  belong to different windows.
- **`GlobalList::bind` takes a version range, not a global name.**
- **The session lock is a `flock`, and its file is never unlinked.** Unlinking it
  would let a second daemon create a fresh file and lock that instead, and both
  would believe they hold the session.
- **An instance removes only its own socket and metadata.** The runtime directory
  is shared with every other session on the machine.
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
# "idle: input stopped 5m ago" on stderr.
```

```sh
# Crash recovery: kill -9 leaves a row open, the next start closes it and prints
# "closed N dangling interval(s)".
```

Inhibitors (a video player holding the screen awake) are the one path that has no
test yet. The protocol comparison is in `src/compositor/idle.rs`; the tracker's
reaction is `Tracker::on_inhibitor`.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/), with a body that
says why rather than what.

```text
<type>(<scope>): <imperative summary>

<what the problem was, what was decided, and what was rejected>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `build`, `ci`,
`chore`. Scope is optional and names the area, as in `fix(ipc):` or
`docs(readme):`. A body that explains the reasoning is the norm here, not an
exception: the reasons a change was made are the part that a diff cannot carry.

BREAKING CHANGE goes in the footer, and a breaking change to the wire protocol
also bumps `INFO_VERSION`.

(The first four commits predate this convention and are left as they are.)

## Packaging

Two AUR packages: `niri-tracker-bin` ships the release tarball, `niri-tracker-git`
builds the tip of `main`. Each AUR repository holds exactly `PKGBUILD` and
`.SRCINFO`; both are generated from `packaging/` and never edited there.

`packaging/SRCINFO-bin` and `SRCINFO-git` are **templates**, not the files that
end up in AUR. The workflow substitutes the version and checksums into them
because the runner is an Ubuntu machine without `makepkg`. After changing either
PKGBUILD, regenerate them on an Arch host and commit the result:

```sh
./packaging/regen-srcinfo.sh
```

`packaging/niri-tracker.service` is installed by both packages and is the same
file the systemd package ships.

## Releasing

The version lives in `Cargo.toml` and nowhere else — there is no `version.go`
equivalent. Tagging is what triggers the release workflow.

```sh
# 1. Bump the version and commit.
# 2. Tag and push.
git tag -a v0.2.0 -m "v0.2.0"
git push origin v0.2.0
```

Three workflows follow the tag, in this order:

1. **Release** builds both targets, uploads the tarballs and their checksums, and
   publishes to crates.io unless that version is already there.
2. **AUR** starts when Release succeeds — not on the tag itself, so it cannot
   race the asset upload — and pushes both packages: `-bin` with the new version
   and checksums, `-git` with a refreshed version snapshot.

Neither publishes a version that disagrees with `Cargo.toml`; both fail loudly
instead. `AUR` can also be run by hand from the Actions tab with a version and a
package choice, which is how a repository that has never been pushed gets
created.

`AUR` needs the `AUR_SSH_PRIVATE_KEY` secret: an ed25519 key registered on the
AUR account and made for this purpose alone, so deleting it later ends the
automation without touching anything else.
