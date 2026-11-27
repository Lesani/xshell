//! What `xshelld pair` reports, shared by the command itself (`xshelld/src/pair.rs`) and the
//! Desktop that runs it on a saved SSH Host ([`super::ssh_host`]): the exit codes and the
//! stderr lines the Desktop tells apart. One definition, never a copied literal.

/// The exit code when nobody entered the code in time.
pub const EXPIRED_EXIT: i32 = 5;
/// The exit code when another device approved this machine but the command ended before the
/// Join was confirmed: xshell on this machine finishes it by itself (never a new offer).
pub const UNFINISHED_EXIT: i32 = 6;
/// The exit code when the Relay refused new pairing attempts from this network for now
/// (HTTP 429, #112): not a connection problem.
pub const RATE_LIMITED_EXIT: i32 = 7;

/// `cli.pair.offline` (stderr, exit 1): the Relay could not be reached.
pub const OFFLINE: &str =
    "Can't reach the service. Check your connection, then run xshelld pair again.";
/// `cli.pair.alreadyRunning` (stderr, exit 1): another `xshelld pair` shows words here.
pub const ALREADY_RUNNING: &str = "Another xshelld pair command is showing words on this machine. Use those words, or stop that command first.";
