//! Socket discovery.
//!
//! One niri session holds one daemon, and a machine can have several sessions at
//! once (two niri logins, or a nested compositor). Each daemon therefore
//! publishes its own pair of files:
//!
//! ```text
//! $XDG_RUNTIME_DIR/niri-tracker/
//! ├── 1234.sock     listening socket, named after the daemon's pid
//! └── 1234.info     key=value metadata, written atomically
//! ```
//!
//! One file per instance rather than a shared registry: a registry needs a lock
//! and a read-modify-write cycle, and two sessions starting at the same moment
//! can leave it inconsistent. Per-instance files have no contention, and
//! recovering from a crash means deleting that instance's own leftovers.
//!
//! Naming the socket after the pid also gives a new instance a fresh path, so
//! there is no "unlink then bind" race with a dying predecessor.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Protocol version of the `.info` file. Bumped only on incompatible changes;
/// readers skip files they do not understand instead of guessing.
pub const INFO_VERSION: u32 = 1;

/// Metadata a daemon publishes next to its socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceInfo {
    /// Which Wayland session this serves, normalised to a basename.
    pub wayland_display: String,
    /// Absolute path to the socket.
    pub socket: PathBuf,
    /// Daemon process id.
    pub pid: u32,
    /// The daemon's start time, in Unix seconds, so the newest can be chosen.
    pub started: i64,
}

impl InstanceInfo {
    /// Serialize as `key=value` lines.
    #[must_use]
    pub fn to_file_contents(&self) -> String {
        format!(
            "version={INFO_VERSION}\n\
             wayland_display={}\n\
             socket={}\n\
             pid={}\n\
             started={}\n",
            self.wayland_display,
            self.socket.display(),
            self.pid,
            self.started,
        )
    }

    /// Parse the `key=value` format.
    ///
    /// Returns `None` for anything unparseable or from a future version: a client
    /// skips a file it does not understand rather than failing the whole lookup.
    #[must_use]
    pub fn parse(contents: &str) -> Option<Self> {
        let mut version = None;
        let mut wayland_display = None;
        let mut socket = None;
        let mut pid = None;
        let mut started = None;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            match key.trim() {
                "version" => version = value.trim().parse::<u32>().ok(),
                "wayland_display" => wayland_display = Some(value.trim().to_owned()),
                "socket" => socket = Some(PathBuf::from(value.trim())),
                "pid" => pid = value.trim().parse::<u32>().ok(),
                "started" => started = value.trim().parse::<i64>().ok(),
                // Unknown keys are ignored so a newer daemon can add fields.
                _ => {}
            }
        }

        if version? != INFO_VERSION {
            return None;
        }
        Some(Self {
            wayland_display: wayland_display?,
            socket: socket?,
            pid: pid?,
            started: started?,
        })
    }
}

/// Normalise a `WAYLAND_DISPLAY` value to a bare socket name.
///
/// The variable may be a socket name (`wayland-1`) or an absolute path
/// (`/run/user/1000/wayland-1`); both must map to the same identity.
#[must_use]
pub fn normalise_display(value: &str) -> String {
    Path::new(value).file_name().map_or_else(
        || value.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Paths belonging to one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    /// The listening socket.
    pub socket: PathBuf,
    /// The metadata file.
    pub info: PathBuf,
}

impl InstancePaths {
    /// The paths of the instance with this pid.
    #[must_use]
    pub fn for_pid(dir: &Path, pid: u32) -> Self {
        Self {
            socket: dir.join(format!("{pid}.sock")),
            info: dir.join(format!("{pid}.info")),
        }
    }

    /// Write the metadata file atomically.
    ///
    /// Written to a temporary name and renamed, so a client never observes a
    /// half-written file.
    pub fn write_info(&self, info: &InstanceInfo) -> std::io::Result<()> {
        if let Some(parent) = self.info.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.info.with_extension("info.tmp");
        {
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(info.to_file_contents().as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &self.info)
    }

    /// Remove both files, ignoring "already gone".
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.info);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// An instance that was found on disk.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// What the instance published about itself.
    pub info: InstanceInfo,
    /// Whether its socket accepted a connection. A stale entry is reported with
    /// `false` rather than silently dropped, so the caller decides what to do.
    pub alive: bool,
}

/// Find every instance serving `display`, newest first.
#[must_use]
pub fn discover(dir: &Path, display: &str) -> Vec<Discovered> {
    let wanted = normalise_display(display);
    let mut found = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "info") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(info) = InstanceInfo::parse(&contents) else {
            continue;
        };
        if info.wayland_display != wanted {
            continue;
        }

        // A socket that refuses a connection belongs to a daemon that is gone;
        // its files are leftovers to be cleaned up.
        let alive = std::os::unix::net::UnixStream::connect(&info.socket).is_ok();
        found.push(Discovered { info, alive });
    }

    // Newest first, so a client picking one of several instances gets the daemon
    // that started last.
    found.sort_by_key(|entry| std::cmp::Reverse(entry.info.started));
    found
}

/// An exclusive claim on one Wayland session's daemon slot.
///
/// The pid-named files describe instances but cannot decide between two that
/// start at the same moment: both would find no live instance and both would
/// publish their own socket. A lock does decide, and the kernel releases it when
/// the process dies, including on a panic, which the files cannot promise.
///
/// The lock file is never removed. Unlinking it would let a second process create
/// a fresh file and lock that instead, and both would believe they hold the slot.
pub struct Lock {
    #[expect(
        dead_code,
        reason = "held for its side effect: the flock lives as long as the file is open"
    )]
    file: std::fs::File,
}

/// Why the slot could not be claimed.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another daemon holds it.
    #[error("already held by pid {pid:?}")]
    Held {
        /// The pid recorded by the holder, when it could be read.
        pid: Option<u32>,
    },

    /// The lock file could not be opened or read.
    #[error("could not open the lock file: {0}")]
    Io(#[from] std::io::Error),
}

impl Lock {
    /// Claim the slot for `display`, or report who holds it.
    pub fn acquire(dir: &std::path::Path, display: &str) -> Result<Self, LockError> {
        use std::io::{Read as _, Seek as _, Write as _};

        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.lock", normalise_display(display)));

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // SAFETY: `flock` only reads the descriptor it is given, which is valid for
        // as long as `file` is open, and takes no other pointers.
        let claimed = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if claimed != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                return Err(LockError::Held {
                    pid: holder.trim().parse().ok(),
                });
            }
            return Err(LockError::Io(err));
        }

        // Recorded for the error message the next daemon prints; not authoritative,
        // because the lock is.
        let _ = file.set_len(0);
        let _ = file.seek(std::io::SeekFrom::Start(0));
        let _ = write!(file, "{}", std::process::id());

        Ok(Self { file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_round_trips() {
        let info = InstanceInfo {
            wayland_display: String::from("wayland-1"),
            socket: PathBuf::from("/run/user/1000/niri-tracker/42.sock"),
            pid: 42,
            started: 1_700_000_000,
        };
        let parsed = InstanceInfo::parse(&info.to_file_contents()).expect("parses");
        assert_eq!(parsed, info);
    }

    #[test]
    fn a_future_version_is_skipped() {
        let text = "version=99\nwayland_display=wayland-1\nsocket=/tmp/x\npid=1\nstarted=0\n";
        assert_eq!(InstanceInfo::parse(text), None);
    }

    #[test]
    fn missing_fields_are_skipped() {
        assert_eq!(
            InstanceInfo::parse("version=1\nwayland_display=wayland-1\n"),
            None
        );
    }

    #[test]
    fn a_display_path_normalises_to_its_basename() {
        assert_eq!(normalise_display("wayland-1"), "wayland-1");
        assert_eq!(normalise_display("/run/user/1000/wayland-1"), "wayland-1");
    }

    #[test]
    fn the_lock_admits_one_holder_at_a_time() {
        let dir = std::env::temp_dir().join(format!("nt-lock-{}", std::process::id()));

        let first = Lock::acquire(&dir, "wayland-1").expect("first claim");
        match Lock::acquire(&dir, "wayland-1") {
            Err(LockError::Held { pid }) => {
                assert_eq!(pid, Some(std::process::id()), "the holder records its pid")
            }
            Ok(_) => panic!("a second daemon must not claim the same session"),
            Err(other) => panic!("unexpected failure: {other}"),
        }

        // Releasing is what the kernel does when the holder goes away.
        drop(first);
        let third = Lock::acquire(&dir, "wayland-1").expect("claim after release");
        drop(third);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_is_per_session() {
        let dir = std::env::temp_dir().join(format!("nt-lock-two-{}", std::process::id()));

        let one = Lock::acquire(&dir, "wayland-1").expect("first session");
        let other = Lock::acquire(&dir, "wayland-2").expect("a different session is separate");

        drop((one, other));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_only_its_own_files() {
        // The runtime directory is shared by every session on the machine, so one
        // instance going away must not take another instance's files with it.
        let dir = std::env::temp_dir().join(format!("nt-discovery-{}", std::process::id()));
        let ours = InstancePaths::for_pid(&dir, 1);
        let theirs = InstancePaths::for_pid(&dir, 2);

        for paths in [&ours, &theirs] {
            std::fs::create_dir_all(&dir).expect("creates the directory");
            std::fs::write(&paths.socket, b"").expect("writes a socket file");
            std::fs::write(&paths.info, b"").expect("writes an info file");
        }

        ours.cleanup();

        assert!(!ours.socket.exists(), "our socket goes");
        assert!(!ours.info.exists(), "our info goes");
        assert!(
            theirs.socket.exists(),
            "the other instance keeps its socket"
        );
        assert!(theirs.info.exists(), "the other instance keeps its info");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_are_named_after_the_pid() {
        let paths = InstancePaths::for_pid(Path::new("/run"), 7);
        assert_eq!(paths.socket, PathBuf::from("/run/7.sock"));
        assert_eq!(paths.info, PathBuf::from("/run/7.info"));
    }
}
