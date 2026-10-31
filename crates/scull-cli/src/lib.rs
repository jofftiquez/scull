//! The `scull` command-line interface (PRD §12).
//!
//! The binary in `main.rs` is a thin wrapper; everything testable lives here:
//!
//! * [`cli`]: the clap command tree and global flags.
//! * [`commands`]: dispatch, and the tracking issue of every command not implemented yet.
//! * [`cmd`]: the implemented commands (`doctor`, ...).
//! * [`db`]: opening the database without creating or migrating it.
//! * [`output`]: tables, byte and tier formatting, terminal escaping (SAFE-17), and the
//!   versioned JSON/NDJSON envelopes (CLI-01).
//! * [`cancel`]: Ctrl-C handling (CLI-02).

pub mod cancel;
pub mod cli;
pub mod cmd;
pub mod commands;
pub mod db;
pub mod output;
