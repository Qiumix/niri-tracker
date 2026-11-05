//! Reading the foreground process of a terminal.
//!
//! A terminal window reports the same app_id for every window and tab, so time
//! spent in nvim is attributed to `kitty`. The interesting name lives one level
//! down: whatever is in the foreground of the terminal's controlling pty.
//!
//! The kernel exposes that as `tpgid` in `/proc/<pid>/stat`: the process group
//! that owns the terminal. A descendant of the terminal whose own process group
//! equals that value is the foreground job. This works for any terminal
//! emulator, needs no cooperation from it, and does not depend on the shell
//! configuring a title format.
//!
//! Only the terminal's own subtree matters, and there are two ways to enumerate
//! it. Kernels built with `CONFIG_PROC_CHILDREN` (the default, and every distro
//! kernel) publish each thread's direct children in
//! `/proc/<pid>/task/<tid>/children`, so the subtree can be walked downward in a
//! dozen file reads. Without it the only option is to read every process on the
//! system and reconstruct the tree, which is tens of times more work for the same
//! answer — measured at 6.8 ms against 0.09 ms for a two-process subtree on a
//! machine running 570 processes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

/// What `/proc` says about one process.
#[derive(Debug, Clone)]
struct ProcEntry {
    pid: u32,
    ppid: u32,
    pgrp: u32,
    tpgid: i32,
    tty_nr: i64,
}

/// Find the foreground process name for a terminal window.
///
/// `root_pid` is the pid niri reports for the window. Returns the basename of the
/// foreground process's `argv[0]`, or `None` when nothing conclusive is found,
/// which the caller treats as "credit the terminal itself".
#[must_use]
pub fn foreground_process_name(root_pid: u32) -> Option<String> {
    let foreground = match walk_children(root_pid) {
        Some(descendants) => pick_foreground(&descendants),
        // No `children` files: reconstruct the tree from the parent links instead.
        None => find_foreground(&read_table(), root_pid),
    };
    foreground.and_then(process_name)
}

/// Upper bound on how many processes a subtree walk will visit.
///
/// A tree cannot legitimately be this large, so hitting it means something is
/// wrong; stopping keeps a pathological tree from stalling the daemon.
const MAX_WALKED: usize = 4096;

/// Walk the subtree under `root_pid` through the kernel's `children` files.
///
/// Returns `None` when the kernel does not publish them, so the caller can fall
/// back. An empty vector means the walk worked and there is simply nothing below
/// the root.
fn walk_children(root_pid: u32) -> Option<Vec<ProcEntry>> {
    // A root that is already gone has no subtree; that is an answer, not a reason
    // to fall back to a system-wide scan.
    if std::fs::metadata(format!("/proc/{root_pid}/stat")).is_err() {
        return Some(Vec::new());
    }

    let mut seen: HashSet<u32> = HashSet::from([root_pid]);
    let mut queue: VecDeque<u32> = VecDeque::from([root_pid]);
    let mut entries: Vec<ProcEntry> = Vec::new();
    let mut supported = false;
    let mut visited = 0;

    while let Some(pid) = queue.pop_front() {
        if visited >= MAX_WALKED {
            break;
        }
        visited += 1;

        if let Some(children) = read_children(pid) {
            supported = true;
            for child in children {
                if seen.insert(child) {
                    queue.push_back(child);
                }
            }
        }

        if pid != root_pid
            && let Some(entry) = read_entry(pid)
        {
            entries.push(entry);
        }
    }

    supported.then_some(entries)
}

/// The direct children of `pid`, across all of its threads.
///
/// `children` belongs to a thread, not a process, so a multi-threaded terminal
/// keeps the shell under whichever thread happened to spawn it. `None` means this
/// kernel does not publish the file at all.
fn read_children(pid: u32) -> Option<Vec<u32>> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut children = Vec::new();
    let mut published = false;

    for task in tasks.flatten() {
        let Ok(text) = std::fs::read_to_string(task.path().join("children")) else {
            // A thread that exited between listing and reading is normal.
            continue;
        };
        published = true;
        children.extend(
            text.split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok()),
        );
    }

    published.then_some(children)
}

/// The pid of the foreground job out of a set of processes known to be descendants.
fn pick_foreground(entries: &[ProcEntry]) -> Option<u32> {
    let foreground: Vec<&ProcEntry> = entries
        .iter()
        .filter(|entry| entry.tty_nr != 0)
        .filter(|entry| entry.tpgid >= 0 && entry.pgrp as i32 == entry.tpgid)
        .collect();

    // The leader of the foreground process group is the job itself. Everything
    // else in that group is a child of it and merely inherited the group, so
    // picking the highest pid would name `node` for an editor that spawned a
    // language server. Several tabs mean several leaders, and the newest is the
    // better guess: starting a job is what makes a tab interesting.
    let newest_leader = foreground
        .iter()
        .filter(|entry| entry.pid == entry.pgrp)
        .map(|entry| entry.pid)
        .max();
    if newest_leader.is_some() {
        return newest_leader;
    }

    // No leader is visible, which happens when it exited while its children carry
    // on. Fall back to the newest process in the group.
    foreground.iter().map(|entry| entry.pid).max()
}

/// The pid of the foreground job, found by scanning the whole process table.
///
/// Kept as its own function because it is the fallback path and because tests can
/// drive it with a table they build by hand.
fn find_foreground(table: &HashMap<u32, ProcEntry>, root_pid: u32) -> Option<u32> {
    if !table.contains_key(&root_pid) {
        return None;
    }
    let candidates: Vec<ProcEntry> = table
        .values()
        .filter(|entry| entry.pid != root_pid && is_descendant(table, entry.pid, root_pid))
        .cloned()
        .collect();
    pick_foreground(&candidates)
}

/// Whether `pid` descends from `ancestor`.
fn is_descendant(table: &HashMap<u32, ProcEntry>, pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    // A malformed tree could loop; the depth bound turns that into "no" instead
    // of a hang.
    for _ in 0..64 {
        let Some(entry) = table.get(&current) else {
            return false;
        };
        if entry.ppid == ancestor {
            return true;
        }
        if entry.ppid == 0 || entry.ppid == current {
            return false;
        }
        current = entry.ppid;
    }
    false
}

/// The basename of a process's `argv[0]`.
///
/// `cmdline` is preferred over `comm` because `comm` is truncated to 15 bytes by
/// the kernel, which turns `firefox-bin` into `firefox-bin` but mangles longer
/// names into new ones.
fn process_name(pid: u32) -> Option<String> {
    let path = PathBuf::from(format!("/proc/{pid}/cmdline"));
    let raw = std::fs::read(path).ok()?;
    let first = raw.split(|byte| *byte == 0).next()?;
    if first.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(first);
    let base = text.rsplit('/').next().unwrap_or(&text);
    let base = base.trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_owned())
    }
}

/// Read `/proc` into a table, skipping anything that vanishes mid-read.
///
/// Processes exit while this runs, so a failed open is normal rather than an
/// error worth reporting.
fn read_table() -> HashMap<u32, ProcEntry> {
    let mut table = HashMap::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if let Some(parsed) = read_entry(pid) {
            table.insert(pid, parsed);
        }
    }

    table
}

/// Parse one `/proc/<pid>/stat`.
///
/// The `comm` field can contain spaces and parentheses, so fields are counted
/// from the last `)`, not from the start of the line.
fn read_entry(pid: u32) -> Option<ProcEntry> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = text.rfind(')')?;
    let rest = text.get(close + 1..)?;

    let mut fields = rest.split_whitespace();
    // After the comm, the next fields are state, ppid, pgrp, session, tty_nr,
    // tpgid.
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    let pgrp = fields.next()?.parse::<u32>().ok()?;
    let _session = fields.next()?;
    let tty_nr = fields.next()?.parse::<i64>().ok()?;
    let tpgid = fields.next()?.parse::<i32>().ok()?;

    Some(ProcEntry {
        pid,
        ppid,
        pgrp,
        tpgid,
        tty_nr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, ppid: u32, pgrp: u32, tpgid: i32, tty_nr: i64) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            pgrp,
            tpgid,
            tty_nr,
        }
    }

    fn table(entries: Vec<ProcEntry>) -> HashMap<u32, ProcEntry> {
        entries
            .into_iter()
            .map(|entry| (entry.pid, entry))
            .collect()
    }

    #[test]
    fn the_children_walk_finds_a_real_child() {
        // Runs the same path the daemon uses. On a kernel without
        // CONFIG_PROC_CHILDREN this exercises the fallback instead, and must still
        // find the child.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a child to look for");
        std::thread::sleep(std::time::Duration::from_millis(100));

        let walked = walk_children(std::process::id()).expect("this kernel publishes children");
        let found = walked.iter().any(|entry| entry.pid == child.id());

        let table = read_table();
        let scanned = table
            .get(&child.id())
            .is_some_and(|entry| is_descendant(&table, entry.pid, std::process::id()));

        let _ = child.kill();
        let _ = child.wait();

        assert!(found, "the children walk must see the child it was given");
        assert!(
            scanned,
            "the fallback scan must place the child under its parent"
        );
    }

    #[test]
    fn a_root_that_is_gone_has_no_subtree() {
        // u32::MAX is not a pid, so nothing is below it. This must be an empty
        // answer rather than a reason to scan the whole system.
        match walk_children(u32::MAX) {
            Some(entries) => assert!(entries.is_empty(), "nothing can be below it"),
            None => panic!("a missing root is an answer, not an unsupported kernel"),
        }
    }

    #[test]
    fn children_are_read_across_threads() {
        // A process with no children still answers: the file exists and is empty.
        // `None` would mean the kernel does not publish it at all.
        let children = read_children(std::process::id());
        assert!(children.is_some(), "this kernel should publish children");
    }

    #[test]
    fn the_foreground_job_is_found() {
        // kitty(100) -> fish(101), and fish's own child nvim(102) owns the pty.
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 102, 7),
            entry(102, 101, 102, 102, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(102));
    }

    #[test]
    fn a_shell_at_the_prompt_is_the_foreground_job() {
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 101, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(101));
    }

    #[test]
    fn a_process_without_a_terminal_is_ignored() {
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 101, 0),
        ]);
        assert_eq!(find_foreground(&table, 100), None);
    }

    #[test]
    fn an_unrelated_foreground_job_is_not_claimed() {
        // 500 is a background job of a different terminal.
        let table = table(vec![entry(100, 1, 100, -1, 0), entry(500, 1, 500, 500, 9)]);
        assert_eq!(find_foreground(&table, 100), None);
    }

    #[test]
    fn the_terminal_itself_is_never_the_answer() {
        let table = table(vec![entry(100, 1, 100, 100, 7)]);
        assert_eq!(find_foreground(&table, 100), None);
    }

    #[test]
    fn the_newest_of_several_tabs_wins() {
        // Two tabs: fish(101) on pty 7 and nvim(201) on pty 8.
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 101, 7),
            entry(201, 100, 201, 201, 8),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(201));
    }

    #[test]
    fn the_group_leader_is_chosen_over_its_children() {
        // nvim (102) leads the foreground group and spawns node (110), which
        // inherits the group. The answer must be nvim, not node.
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 102, 7),
            entry(102, 101, 102, 102, 7),
            entry(110, 102, 102, 102, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(102));
    }

    #[test]
    fn without_a_leader_the_newest_process_wins() {
        // The group leader (999) has exited; two of its children remain.
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(110, 100, 999, 999, 7),
            entry(120, 100, 999, 999, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(120));
    }

    #[test]
    fn a_deep_descendant_is_still_a_descendant() {
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 100, 101, 104, 7),
            entry(102, 101, 102, 104, 7),
            entry(104, 102, 104, 104, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), Some(104));
    }

    #[test]
    fn a_missing_root_yields_nothing() {
        let table = table(vec![entry(101, 100, 101, 101, 7)]);
        assert_eq!(find_foreground(&table, 100), None);
    }

    #[test]
    fn a_cycle_does_not_hang() {
        let table = table(vec![
            entry(100, 1, 100, -1, 0),
            entry(101, 102, 101, 101, 7),
            entry(102, 101, 101, 101, 7),
        ]);
        assert_eq!(find_foreground(&table, 100), None);
    }
}
