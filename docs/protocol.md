# Control protocol

The daemon listens on a Unix socket and speaks one JSON object per line. It is
deliberately plain: `nc -U` piped into `jq` is enough to drive it by hand, which
matters more than saving bytes on a socket that sees a few messages a minute.

## Finding the socket

```text
$XDG_RUNTIME_DIR/niri-tracker/
├── 1234.sock     listening socket, named after the daemon's pid
└── 1234.info     key=value metadata, written atomically
```

`NIRI_TRACKER_RUNTIME_DIR` overrides the directory. One daemon per Wayland
session; several sessions on one machine each get their own files, and the pid in
the name means a new instance always gets a fresh path with no bind race against
a dying predecessor.

The `.info` file:

```text
version=1
wayland_display=wayland-1
socket=/run/user/1000/niri-tracker/1234.sock
pid=1234
started=1767225600
```

`version` is the format version; a reader skips a file it does not understand
rather than guessing. `started` is Unix seconds, and a client picking among
several live instances takes the newest.

A socket that refuses a connection belongs to a daemon that is gone; its files are
leftovers.

## Request

One JSON object, one line, ending in `\n`:

```json
{"id": 1, "verb": "status"}
```

- `id` — client-assigned, echoed in the reply. Any number.
- `verb` — one of the verbs below; an unknown verb is a parse error.
- `args` — present only for verbs that take arguments.

### Verbs

| Verb | Arguments | Effect |
|---|---|---|
| `status` | — | What the daemon is doing right now |
| `pause` | — | Stop recording |
| `resume` | — | Resume recording |
| `reload` | — | Re-read the configuration |
| `quit` | — | Ask the daemon to exit |
| `report` | query | Totals grouped as asked |
| `details` | query | Window titles of one application |

### Query arguments

Used by `report` and `details`:

```json
{
  "from": "2026-11-01",
  "to": "2026-11-05",
  "by": "app",
  "exclude_passive": false
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `from` | `YYYY-MM-DD` | yes | First local day to include |
| `to` | `YYYY-MM-DD` | yes | Last local day to include, **inclusive** |
| `by` | `app` \| `title` \| `day` \| `hour` | yes | How rows are grouped |
| `app` | string | no | Only this credited name, matched exactly |
| `title` | string | no | Only titles containing this text |
| `exclude_passive` | bool | no | Leave out time an idle inhibitor covered |
| `limit` | number | no | Keep at most this many rows; totals still cover everything |

`details` is `report --by title` with an `app` filter, but it is a separate verb
so the intent is explicit and the grouping cannot be accidentally overridden.

## Response

A successful reply:

```json
{"id": 1, "result": {"paused": false, "pid": 1234, "intervals": 41, "live": {"app_id": "nvim", "title": "design.md", "seconds": 92, "passive": false}}}
```

A failed one:

```json
{"id": 1, "error": {"kind": "bad_request", "message": "report needs a query"}}
```

`kind` is one of `bad_request`, `unavailable`, `storage`, `internal`; `message` is
for a human.

`result` is one of three shapes, distinguished by their fields:

**Status** — as above. `live` is absent when nothing is focused or when recording
is paused.

**Report**:

```json
{"rows": [{"name": "nvim", "seconds": 4321}], "total_seconds": 4321}
```

`rows` is largest first, ties broken alphabetically so runs are reproducible.
`total_seconds` covers everything counted, **before** `limit` hid any rows.

**Acknowledgement** — `null`, for verbs that change something and have nothing to
report.

## Examples

```sh
SOCK="$XDG_RUNTIME_DIR/niri-tracker/$(ls "$XDG_RUNTIME_DIR/niri-tracker" | sed -n 's/\.info$//p' | head -1).sock"

# Is it recording?
printf '{"id":1,"verb":"status"}\n' | nc -U "$SOCK" | jq

# What did I do last week?
printf '{"id":2,"verb":"report","args":{"from":"2026-10-27","to":"2026-11-02","by":"app"}}\n' \
  | nc -U "$SOCK" | jq '.result.rows'

# Which titles did I have open in kitty today?
printf '{"id":3,"verb":"details","args":{"from":"2026-11-05","to":"2026-11-05","by":"title","app":"kitty"}}\n' \
  | nc -U "$SOCK" | jq '.result.rows'
```

## Stability

The protocol is versioned by the `.info` file's `version` field, which is bumped
only on incompatible changes. Additive changes — new verbs, new optional fields —
do not bump it: a reader that does not know a verb is expected to fail that one
request rather than the connection.

Both ends ship in the same binary, so a mismatch means two versions are installed
at once, which the daemon's refusal to be the second instance per session makes
unlikely.
