//! niri-tracker: screen time tracking for the niri Wayland compositor.
//!
//! The binary is both the daemon that watches the compositor and the client
//! that reports what it recorded. All state lives in one SQLite file, and the
//! daemon is its only writer; clients ask the daemon rather than reading it, so
//! a report and `status` can never disagree.

pub mod cli;
pub mod config;
pub mod db;
pub mod ipc;
pub mod report;
pub mod terminal;
pub mod tracker;
